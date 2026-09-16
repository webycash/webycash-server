//! `LedgerStore<A>` for the Cloudflare Containers deployment path.
//!
//! The Container runs the native hyper-based server. D1's data-plane is
//! only exposed to Workers, not to arbitrary outbound HTTP clients, so
//! the Container reaches it by HTTP-calling a thin TS Worker that holds
//! the binding. Each `LedgerStore` method serializes a batch of SQL
//! statements as JSON, POSTs them to `{worker_url}/__d1/batch`, and
//! parses the JSON response. The Worker's only job is to forward
//! `db.batch(stmts)` and return rows / metadata.
//!
//! Dialect: SQL constants live in [`crate::storage::d1_sql`]. The
//! trigger-based atomicity primitives in the migration
//! (`BEFORE UPDATE OF spent` + the `INSTEAD OF INSERT` view trigger on
//! `assert_token_exists`) carry double-spend and missing-input
//! enforcement inside `db.batch(...)`.
//!
//! Auth: optional `Authorization: Bearer <token>` header for the
//! Container ↔ Worker hop. The Worker should reject unauthenticated
//! `/__d1/*` calls in production deployments — the binding is internal.

use std::collections::HashMap;
use std::marker::PhantomData;

use crate::asset_core::Asset;
use crate::storage::d1_sql::{
    ASSERT_TOKEN_EXISTS, INSERT_AUDIT, INSERT_TOKEN, READ_CHUNK, SELECT_MINING_STATE,
    UPDATE_SPENT, UPSERT_MINING_STATE,
};
use crate::storage::{
    BurnRecord, HashRecord, KeyStrategy, LedgerStore, MiningState, Namespace,
    ReplaceOp, ReplaceResult,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Native `LedgerStore<A>` implementation that proxies all D1 ops to a
/// Cloudflare Worker over HTTP.
pub struct WorkerProxyStore<A: Asset, K: KeyStrategy> {
    client: reqwest::Client,
    worker_url: String,
    auth_token: Option<String>,
    keys: K,
    _ph: PhantomData<A>,
}

impl<A: Asset, K: KeyStrategy> WorkerProxyStore<A, K> {
    /// Build a new proxy store.
    ///
    /// `worker_url` is the base URL of the Worker (e.g.
    /// `https://webycash-webcash.example.workers.dev` or, in `wrangler
    /// dev` local mode, `http://127.0.0.1:8787`). The store POSTs to
    /// `{worker_url}/__d1/batch`.
    pub fn new(worker_url: String, auth_token: Option<String>, keys: K) -> Self {
        Self {
            client: reqwest::Client::builder()
                .pool_idle_timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            worker_url: worker_url.trim_end_matches('/').to_string(),
            auth_token,
            keys,
            _ph: PhantomData,
        }
    }

    async fn batch(&self, stmts: Vec<BatchStmt>) -> anyhow::Result<Vec<BatchResult>> {
        let url = format!("{}/__d1/batch", self.worker_url);
        let req = self.client.post(&url).json(&BatchRequest { statements: stmts });
        let req = if let Some(t) = &self.auth_token {
            req.header("Authorization", format!("Bearer {t}"))
        } else {
            req
        };
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("worker proxy POST: {e}"))?;
        let status = resp.status();
        let body: BatchResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("worker proxy decode (status={status}): {e}"))?;
        if !body.ok {
            return Err(anyhow::anyhow!(
                "{}",
                body.error.unwrap_or_else(|| "unknown".into())
            ));
        }
        Ok(body.results.unwrap_or_default())
    }
}

#[async_trait]
impl<A: Asset, K: KeyStrategy> LedgerStore<A> for WorkerProxyStore<A, K>
where
    A::Record: HashRecord,
{
    async fn insert_tokens(&self, records: &[A::Record]) -> anyhow::Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut stmts = Vec::with_capacity(records.len());
        for r in records {
            let pk = self
                .keys
                .token_key(A::NAME, &r.namespace(), r.public_hash());
            let data = record_to_data_json(r);
            stmts.push(BatchStmt {
                sql: INSERT_TOKEN.into(),
                params: vec![
                    serde_json::Value::from(pk),
                    serde_json::Value::from(r.amount_wats()),
                    serde_json::Value::from(data),
                ],
            });
        }
        self.batch(stmts).await.map(|_| ())
    }

    async fn get_tokens(
        &self,
        ns: &Namespace,
        hashes: &[String],
    ) -> anyhow::Result<Vec<Option<A::Record>>> {
        if hashes.is_empty() {
            return Ok(vec![]);
        }
        let pks: Vec<String> = hashes
            .iter()
            .map(|h| self.keys.token_key(A::NAME, ns, h))
            .collect();
        let mut by_pk: HashMap<String, (bool, String)> = HashMap::with_capacity(pks.len());
        for chunk in pks.chunks(READ_CHUNK) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql =
                format!("SELECT pk, spent, data FROM tokens WHERE pk IN ({placeholders})");
            let params: Vec<serde_json::Value> = chunk
                .iter()
                .map(|s| serde_json::Value::from(s.clone()))
                .collect();
            let results = self.batch(vec![BatchStmt { sql, params }]).await?;
            for row in results
                .into_iter()
                .next()
                .map(|r| r.rows)
                .unwrap_or_default()
            {
                let pk = row
                    .get("pk")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let spent = row
                    .get("spent")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
                    != 0;
                let data = row
                    .get("data")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}")
                    .to_string();
                by_pk.insert(pk, (spent, data));
            }
        }
        Ok(pks
            .iter()
            .zip(hashes.iter())
            .map(|(pk, h)| {
                by_pk
                    .get(pk)
                    .and_then(|(spent, data)| data_json_to_record::<A>(h, *spent, data))
            })
            .collect())
    }

    async fn check_tokens(
        &self,
        ns: &Namespace,
        hashes: &[String],
    ) -> anyhow::Result<Vec<(String, Option<bool>)>> {
        if hashes.is_empty() {
            return Ok(vec![]);
        }
        let pks: Vec<String> = hashes
            .iter()
            .map(|h| self.keys.token_key(A::NAME, ns, h))
            .collect();
        let mut by_pk: HashMap<String, bool> = HashMap::with_capacity(pks.len());
        for chunk in pks.chunks(READ_CHUNK) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("SELECT pk, spent FROM tokens WHERE pk IN ({placeholders})");
            let params: Vec<serde_json::Value> = chunk
                .iter()
                .map(|s| serde_json::Value::from(s.clone()))
                .collect();
            let results = self.batch(vec![BatchStmt { sql, params }]).await?;
            for row in results
                .into_iter()
                .next()
                .map(|r| r.rows)
                .unwrap_or_default()
            {
                let pk = row
                    .get("pk")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let spent = row
                    .get("spent")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
                    != 0;
                by_pk.insert(pk, spent);
            }
        }
        Ok(pks
            .into_iter()
            .zip(hashes.iter())
            .map(|(pk, h)| (h.clone(), by_pk.get(&pk).copied()))
            .collect())
    }

    async fn batch_replace(
        &self,
        ns: &Namespace,
        ops: &[ReplaceOp<A::Record>],
    ) -> Vec<ReplaceResult> {
        let mut out = Vec::with_capacity(ops.len());
        for op in ops {
            let mut stmts: Vec<BatchStmt> =
                Vec::with_capacity(op.inputs.len() * 2 + op.outputs.len() + 1);
            for h in &op.inputs {
                let pk = self.keys.token_key(A::NAME, ns, h);
                stmts.push(BatchStmt {
                    sql: ASSERT_TOKEN_EXISTS.into(),
                    params: vec![serde_json::Value::from(pk.clone())],
                });
                stmts.push(BatchStmt {
                    sql: UPDATE_SPENT.into(),
                    params: vec![serde_json::Value::from(pk)],
                });
            }
            for r in &op.outputs {
                let pk = self.keys.token_key(A::NAME, ns, r.public_hash());
                let data = record_to_data_json(r);
                stmts.push(BatchStmt {
                    sql: INSERT_TOKEN.into(),
                    params: vec![
                        serde_json::Value::from(pk),
                        serde_json::Value::from(r.amount_wats()),
                        serde_json::Value::from(data),
                    ],
                });
            }
            let audit_pk = self.keys.replacement_key(A::NAME, ns, &op.record.id);
            let audit = replacement_audit_json(&op.record);
            stmts.push(BatchStmt {
                sql: INSERT_AUDIT.into(),
                params: vec![
                    serde_json::Value::from(audit_pk),
                    serde_json::Value::from(audit),
                ],
            });
            out.push(match self.batch(stmts).await {
                Ok(_) => ReplaceResult::Ok,
                Err(e) => ReplaceResult::Failed(diagnostic_from_error(&e.to_string())),
            });
        }
        out
    }

    async fn batch_burn(
        &self,
        ns: &Namespace,
        ops: &[(String, BurnRecord)],
    ) -> anyhow::Result<()> {
        for (hash, record) in ops {
            let pk = self.keys.token_key(A::NAME, ns, hash);
            let audit_pk = self.keys.burn_key(A::NAME, ns, &record.id);
            let audit = burn_audit_json(record);
            let stmts = vec![
                BatchStmt {
                    sql: ASSERT_TOKEN_EXISTS.into(),
                    params: vec![serde_json::Value::from(pk.clone())],
                },
                BatchStmt {
                    sql: UPDATE_SPENT.into(),
                    params: vec![serde_json::Value::from(pk)],
                },
                BatchStmt {
                    sql: INSERT_AUDIT.into(),
                    params: vec![
                        serde_json::Value::from(audit_pk),
                        serde_json::Value::from(audit),
                    ],
                },
            ];
            self.batch(stmts).await?;
        }
        Ok(())
    }

    async fn get_mining_state(&self) -> anyhow::Result<Option<MiningState>> {
        let pk = self.keys.mining_state_key(A::NAME);
        let stmts = vec![BatchStmt {
            sql: SELECT_MINING_STATE.into(),
            params: vec![serde_json::Value::from(pk)],
        }];
        let results = self.batch(stmts).await?;
        let row = results
            .into_iter()
            .next()
            .and_then(|r| r.rows.into_iter().next());
        Ok(row
            .and_then(|r| r.get("data").and_then(|v| v.as_str().map(|s| s.to_string())))
            .as_deref()
            .and_then(parse_mining_state))
    }

    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()> {
        let pk = self.keys.mining_state_key(A::NAME);
        let data = mining_state_json(state);
        let stmts = vec![BatchStmt {
            sql: UPSERT_MINING_STATE.into(),
            params: vec![
                serde_json::Value::from(pk),
                serde_json::Value::from(data),
            ],
        }];
        self.batch(stmts).await.map(|_| ())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JSON envelopes — kept private. Wire format with the Worker:
//
//   POST {worker_url}/__d1/batch
//   {"statements": [{"sql": "...", "params": [...]}, ...]}
//   →
//   200 {"ok": true,  "results": [{"rows": [...], "meta": {...}}, ...]}
//   4xx {"ok": false, "error": "..."}
//
// The Worker (deploy/cloudflare-container/workers/{flavor}/src/index.ts) translates
// each statement into D1's `prepare(sql).bind(params)` + `.all()`, packs
// them into `D1Database.batch([...])`, and returns the row arrays.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct BatchRequest {
    statements: Vec<BatchStmt>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct BatchStmt {
    sql: String,
    params: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct BatchResponse {
    ok: bool,
    #[serde(default)]
    results: Option<Vec<BatchResult>>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize, Default)]
struct BatchResult {
    #[serde(default)]
    rows: Vec<HashMap<String, serde_json::Value>>,
    // `meta.changes` etc. are returned by D1 but the backend doesn't act
    // on them; the trigger-based primitives in the schema are what carry
    // atomicity. Reserved for future use.
}

// ─────────────────────────────────────────────────────────────────────────────
// Serialization helpers — local copies of the ones in `d1_backend.rs`. Kept
// duplicated rather than re-exported because `d1_backend.rs` is gated on
// the `d1` cargo feature and we'd have to enable both features together
// just to reach them.
// ─────────────────────────────────────────────────────────────────────────────

fn record_to_data_json<R: HashRecord>(record: &R) -> String {
    let mut fields: HashMap<String, String> = HashMap::new();
    record.to_fields(&mut fields);
    serde_json::to_string(&fields).unwrap_or_else(|_| "{}".into())
}

fn data_json_to_record<A: Asset>(
    public_hash: &str,
    spent: bool,
    data_json: &str,
) -> Option<A::Record>
where
    A::Record: HashRecord,
{
    let mut fields: HashMap<String, String> = serde_json::from_str(data_json).ok()?;
    fields.insert("spent".into(), if spent { "1" } else { "0" }.into());
    <A::Record as HashRecord>::from_fields(public_hash, &fields)
}

fn replacement_audit_json(record: &crate::storage::ReplacementRecord) -> String {
    serde_json::to_string(record).unwrap_or_else(|_| "{}".into())
}

fn burn_audit_json(record: &BurnRecord) -> String {
    serde_json::to_string(record).unwrap_or_else(|_| "{}".into())
}

fn mining_state_json(state: &MiningState) -> String {
    serde_json::to_string(state).unwrap_or_else(|_| "{}".into())
}

fn parse_mining_state(json: &str) -> Option<MiningState> {
    serde_json::from_str(json).ok()
}

fn diagnostic_from_error(err: &str) -> String {
    if err.contains("double_spend") {
        "double_spend".into()
    } else if err.contains("input_missing") {
        "input_missing".into()
    } else if err.contains("UNIQUE constraint failed") || err.contains("constraint failed") {
        "output_collision".into()
    } else {
        err.to_string()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// In-process tests — spin up a hyper server backed by `rusqlite` that
// implements the same `/__d1/batch` contract the production TS Worker
// exposes. This proves both halves of the wire protocol round-trip
// cleanly and that the SQL semantics (trigger-based atomicity, PK
// collision rollback) survive the JSON serialization hop.
//
// The real end-to-end story (wrangler dev + Container + TS Worker) is
// exercised by `crates/conformance/tests/server_*_container_d1_lifecycle.rs`.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_core::{Asset, AssetPublic, RecordBuilder, RecordOrigin};
    use crate::asset_webcash::Webcash;
    use crate::storage::{
        BurnRecord, MiningState, Namespace, ReplaceOp, ReplaceResult, ReplacementRecord,
        WebcashLegacyKeys,
    };
    use chrono::Utc;
    use http_body_util::{BodyExt, Full};
    use hyper::body::{Bytes, Incoming};
    use hyper::service::service_fn;
    use hyper::{Request, Response, StatusCode};
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::server::conn::auto::Builder;
    use rusqlite::Connection;
    use std::convert::Infallible;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    const SCHEMA: &str =
        include_str!("../../../deploy/cloudflare-container/migrations/0001_initial.sql");

    /// Spawn an in-process hyper server that speaks the `/__d1/batch`
    /// contract on top of an in-memory `rusqlite::Connection`. Returns
    /// the bound base URL ("http://127.0.0.1:<port>") and the shared
    /// connection so individual tests can pre-seed state if they want.
    async fn spawn_worker() -> (String, Arc<Mutex<Connection>>) {
        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        conn.execute_batch(SCHEMA).expect("apply schema");
        let shared = Arc::new(Mutex::new(conn));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral");
        let addr = listener.local_addr().expect("local addr");
        let svc_conn = shared.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let svc_conn = svc_conn.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req: Request<Incoming>| {
                        let svc_conn = svc_conn.clone();
                        async move { Ok::<_, Infallible>(handle(svc_conn, req).await) }
                    });
                    let _ = Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        (format!("http://{addr}"), shared)
    }

    async fn handle(
        conn: Arc<Mutex<Connection>>,
        req: Request<Incoming>,
    ) -> Response<Full<Bytes>> {
        if req.uri().path() != "/__d1/batch" {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
        }
        let body = match req.into_body().collect().await {
            Ok(b) => b.to_bytes(),
            Err(e) => return error_response(&format!("body: {e}")),
        };
        let parsed: BatchRequest = match serde_json::from_slice(&body) {
            Ok(p) => p,
            Err(e) => return error_response(&format!("parse: {e}")),
        };
        let mut results: Vec<BatchResult> = Vec::with_capacity(parsed.statements.len());
        let conn = conn.lock().await;
        let tx = match conn.unchecked_transaction() {
            Ok(t) => t,
            Err(e) => return error_response(&e.to_string()),
        };
        for stmt in &parsed.statements {
            match exec_one(&tx, stmt) {
                Ok(r) => results.push(r),
                Err(e) => {
                    return error_response(&e.to_string());
                }
            }
        }
        match tx.commit() {
            Ok(()) => ok_response(&BatchResponse {
                ok: true,
                results: Some(results),
                error: None,
            }),
            Err(e) => error_response(&e.to_string()),
        }
    }

    fn exec_one(
        tx: &rusqlite::Transaction<'_>,
        stmt: &BatchStmt,
    ) -> rusqlite::Result<BatchResult> {
        // Bind JSON params; rusqlite::params_from_iter accepts ToSql.
        let params: Vec<Box<dyn rusqlite::ToSql>> = stmt
            .params
            .iter()
            .map(json_to_sql)
            .collect();
        let mut prepared = tx.prepare(&stmt.sql)?;
        let column_names: Vec<String> = prepared
            .column_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut rows = prepared.query(rusqlite::params_from_iter(
            params.iter().map(|b| b.as_ref()),
        ))?;
        let mut out_rows: Vec<HashMap<String, serde_json::Value>> = Vec::new();
        while let Some(row) = rows.next()? {
            let mut obj = HashMap::with_capacity(column_names.len());
            for (i, col) in column_names.iter().enumerate() {
                let v: rusqlite::types::Value = row.get(i)?;
                obj.insert(col.clone(), sql_to_json(&v));
            }
            out_rows.push(obj);
        }
        Ok(BatchResult { rows: out_rows })
    }

    fn json_to_sql(v: &serde_json::Value) -> Box<dyn rusqlite::ToSql> {
        match v {
            serde_json::Value::Null => Box::new(Option::<String>::None),
            serde_json::Value::Bool(b) => Box::new(*b as i64),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Box::new(i)
                } else if let Some(f) = n.as_f64() {
                    Box::new(f)
                } else {
                    Box::new(n.to_string())
                }
            }
            serde_json::Value::String(s) => Box::new(s.clone()),
            other => Box::new(other.to_string()),
        }
    }

    fn sql_to_json(v: &rusqlite::types::Value) -> serde_json::Value {
        use rusqlite::types::Value;
        match v {
            Value::Null => serde_json::Value::Null,
            Value::Integer(i) => serde_json::Value::from(*i),
            Value::Real(f) => serde_json::Value::from(*f),
            Value::Text(s) => serde_json::Value::String(s.clone()),
            Value::Blob(b) => {
                serde_json::Value::String(base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    b,
                ))
            }
        }
    }

    fn ok_response(body: &BatchResponse) -> Response<Full<Bytes>> {
        let json = serde_json::to_vec(body).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(json)))
            .unwrap()
    }

    fn error_response(msg: &str) -> Response<Full<Bytes>> {
        let body = BatchResponse {
            ok: false,
            results: None,
            error: Some(msg.to_string()),
        };
        let json = serde_json::to_vec(&body).unwrap_or_default();
        Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(json)))
            .unwrap()
    }

    // Make BatchResponse serializable for the in-test handler.
    impl Serialize for BatchResponse {
        fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeStruct;
            let mut st = s.serialize_struct("BatchResponse", 3)?;
            st.serialize_field("ok", &self.ok)?;
            if let Some(r) = &self.results {
                st.serialize_field("results", r)?;
            }
            if let Some(e) = &self.error {
                st.serialize_field("error", e)?;
            }
            st.end()
        }
    }
    impl Serialize for BatchResult {
        fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeStruct;
            let mut st = s.serialize_struct("BatchResult", 1)?;
            st.serialize_field("rows", &self.rows)?;
            st.end()
        }
    }

    fn record(secret_token: &str) -> <Webcash as Asset>::Record {
        let s = Webcash::parse_secret(secret_token).expect("parse webcash secret");
        <Webcash as RecordBuilder>::record_from_secret(&s, RecordOrigin::Mined)
    }

    fn public_hash(secret_token: &str) -> String {
        let s = Webcash::parse_secret(secret_token).expect("parse");
        Webcash::to_public(&s).public_hash().to_string()
    }

    fn replacement(
        op_id: &str,
        inputs: Vec<String>,
        outputs: &[<Webcash as Asset>::Record],
    ) -> ReplaceOp<<Webcash as Asset>::Record> {
        let output_hashes = outputs.iter().map(|r| r.public_hash().to_string()).collect();
        ReplaceOp {
            inputs: inputs.clone(),
            outputs: outputs.to_vec(),
            record: ReplacementRecord {
                id: op_id.into(),
                input_hashes: inputs,
                output_hashes,
                total_amount_wats: outputs.iter().map(|r| r.amount_wats()).sum(),
                created_at: Utc::now(),
            },
        }
    }

    const A: &str =
        "e1:secret:0000000000000000000000000000000000000000000000000000000000000001";
    const B: &str =
        "e1:secret:0000000000000000000000000000000000000000000000000000000000000002";
    const C: &str =
        "e2:secret:0000000000000000000000000000000000000000000000000000000000000003";

    #[tokio::test]
    async fn insert_and_get_round_trip_over_http() {
        let (url, _conn) = spawn_worker().await;
        let store = WorkerProxyStore::<Webcash, WebcashLegacyKeys>::new(
            url,
            None,
            WebcashLegacyKeys,
        );
        let r = record(A);
        store
            .insert_tokens(std::slice::from_ref(&r))
            .await
            .unwrap();
        let h = public_hash(A);
        let got = store
            .get_tokens(&Namespace::unscoped(), std::slice::from_ref(&h))
            .await
            .unwrap();
        let rec = got[0].as_ref().expect("token present");
        assert_eq!(rec.amount_wats(), r.amount_wats());
        assert_eq!(rec.public_hash(), h);
    }

    #[tokio::test]
    async fn replace_double_spend_propagates_diagnostic_over_http() {
        let (url, _conn) = spawn_worker().await;
        let store = WorkerProxyStore::<Webcash, WebcashLegacyKeys>::new(
            url,
            None,
            WebcashLegacyKeys,
        );
        store.insert_tokens(&[record(A)]).await.unwrap();
        let ha = public_hash(A);
        // First replace consumes A → produces B.
        let _ = store
            .batch_replace(
                &Namespace::unscoped(),
                &[replacement("op-1", vec![ha.clone()], &[record(B)])],
            )
            .await;
        // Second replace tries A again — must come back as Failed with
        // diagnostic carrying `double_spend`.
        let results = store
            .batch_replace(
                &Namespace::unscoped(),
                &[replacement("op-2", vec![ha], &[record(C)])],
            )
            .await;
        match results.as_slice() {
            [ReplaceResult::Failed(d)] => assert!(
                d.contains("double_spend"),
                "expected double_spend diagnostic over HTTP, got: {d}"
            ),
            other => panic!("expected Failed, got {other:?}"),
        }
        // The aborted batch must NOT have inserted C.
        let got = store
            .get_tokens(&Namespace::unscoped(), &[public_hash(C)])
            .await
            .unwrap();
        assert!(got[0].is_none(), "double_spend batch leaked an insert");
    }

    #[tokio::test]
    async fn replace_missing_input_aborts_over_http() {
        let (url, _conn) = spawn_worker().await;
        let store = WorkerProxyStore::<Webcash, WebcashLegacyKeys>::new(
            url,
            None,
            WebcashLegacyKeys,
        );
        // A is never inserted — the assert_token_exists view trigger
        // must fire and abort the batch.
        let results = store
            .batch_replace(
                &Namespace::unscoped(),
                &[replacement(
                    "op-1",
                    vec![public_hash(A)],
                    &[record(B)],
                )],
            )
            .await;
        match results.as_slice() {
            [ReplaceResult::Failed(d)] => assert!(
                d.contains("input_missing"),
                "expected input_missing diagnostic, got: {d}"
            ),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn batch_burn_and_check_round_trip_over_http() {
        let (url, _conn) = spawn_worker().await;
        let store = WorkerProxyStore::<Webcash, WebcashLegacyKeys>::new(
            url,
            None,
            WebcashLegacyKeys,
        );
        store.insert_tokens(&[record(A)]).await.unwrap();
        let h = public_hash(A);
        let burn = BurnRecord {
            id: "burn-1".into(),
            public_hash: h.clone(),
            amount_wats: 100_000_000,
            burned_at: Utc::now(),
        };
        store
            .batch_burn(&Namespace::unscoped(), &[(h.clone(), burn)])
            .await
            .unwrap();
        let check = store
            .check_tokens(&Namespace::unscoped(), &[h])
            .await
            .unwrap();
        assert_eq!(check[0].1, Some(true));
    }

    #[tokio::test]
    async fn mining_state_round_trip_over_http() {
        let (url, _conn) = spawn_worker().await;
        let store = WorkerProxyStore::<Webcash, WebcashLegacyKeys>::new(
            url,
            None,
            WebcashLegacyKeys,
        );
        assert!(store.get_mining_state().await.unwrap().is_none());
        let state = MiningState {
            total_circulation_wats: 12345,
            mining_reports_count: 7,
            difficulty_target_bits: 16,
            ..MiningState::default()
        };
        store.update_mining_state(&state).await.unwrap();
        let back = store.get_mining_state().await.unwrap().unwrap();
        assert_eq!(back.total_circulation_wats, 12345);
        assert_eq!(back.mining_reports_count, 7);
    }
}

