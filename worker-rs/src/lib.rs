//! webycash-server asset flavors on Cloudflare Workers (workers-rs) + D1.
//!
//! The facade (`webycash-server`) exposes runtime-neutral handlers
//! (`route_proto*`) and the `#[async_trait] LedgerStore` algebra. This crate
//! provides:
//!   1. `D1Store` — a `LedgerStore` over `worker::D1Database` (native D1, no
//!      `/__d1` HTTP proxy). Mirrors `worker_proxy_backend` exactly, swapping
//!      the HTTP `batch()` for `D1Database.batch()`.
//!   2. `#[event(fetch)]` — builds a `ProtoRequest`, dispatches via
//!      `route_proto`, converts the `ProtoResponse` to a `worker::Response`.
//!
//! `LedgerStore` is `#[async_trait]` (Send-boxed futures), but `worker`'s D1
//! futures + JsValue are `!Send`. `SendWrapper`/`SendFuture` bridge that on the
//! single-threaded Workers runtime: the store struct is Send+Sync, and each
//! trait-method body runs inside a `SendFuture` so its boxed future is Send.

use std::collections::HashMap;
use std::marker::PhantomData;

use async_trait::async_trait;
use send_wrapper::SendWrapper;
use serde::Deserialize;
use worker::send::SendFuture;
use worker::wasm_bindgen::JsValue;
use worker::*;

use webycash_server::asset_core::Asset;
use webycash_server::asset_webcash::Webcash;
use webycash_server::server_core::{route_proto, ProtoRequest, ProtoResponse, ServeConfig, Server};
use webycash_server::storage::{
    BurnRecord, HashRecord, KeyStrategy, LedgerStore, MiningState, Namespace, ReplaceOp,
    ReplaceResult, WebcashLegacyKeys,
};

// ── D1 SQL (inlined; the facade's `d1_sql` module is private/feature-gated) ──
const INSERT_TOKEN: &str =
    "INSERT INTO tokens (pk, amount_wats, spent, data) VALUES (?1, ?2, 0, ?3)";
const UPDATE_SPENT: &str = "UPDATE tokens SET spent = 1 WHERE pk = ?1";
const ASSERT_TOKEN_EXISTS: &str = "INSERT INTO assert_token_exists (pk) VALUES (?1)";
const INSERT_AUDIT: &str = "INSERT INTO audit_log (pk, data) VALUES (?1, ?2)";
const UPSERT_MINING_STATE: &str =
    "INSERT INTO mining_state (pk, data) VALUES (?1, ?2) ON CONFLICT(pk) DO UPDATE SET data = excluded.data";
const SELECT_MINING_STATE: &str = "SELECT data FROM mining_state WHERE pk = ?1";
const READ_CHUNK: usize = 500;

fn vs(s: &str) -> JsValue {
    JsValue::from_str(s)
}
fn vi(n: i64) -> JsValue {
    JsValue::from_f64(n as f64)
}

// ── D1-backed LedgerStore ──
struct D1Store<A: Asset, K: KeyStrategy> {
    db: SendWrapper<D1Database>,
    keys: K,
    _ph: PhantomData<A>,
}

impl<A: Asset, K: KeyStrategy> D1Store<A, K> {
    fn new(db: D1Database, keys: K) -> Self {
        Self { db: SendWrapper::new(db), keys, _ph: PhantomData }
    }

    /// Run a list of (sql, params) as ONE atomic D1 batch. The schema's
    /// triggers ABORT the whole batch on double-spend / missing-input, which
    /// surfaces here as an `Err` carrying the SQLite message.
    async fn run_batch(&self, stmts: Vec<(&str, Vec<JsValue>)>) -> std::result::Result<(), String> {
        let mut prepared = Vec::with_capacity(stmts.len());
        for (sql, params) in stmts {
            let st = self.db.prepare(sql).bind(&params).map_err(|e| e.to_string())?;
            prepared.push(st);
        }
        self.db.batch(prepared).await.map_err(|e| e.to_string()).map(|_| ())
    }

    async fn query<T: for<'de> Deserialize<'de>>(
        &self,
        sql: &str,
        params: Vec<JsValue>,
    ) -> anyhow::Result<Vec<T>> {
        let res = self
            .db
            .prepare(sql)
            .bind(&params)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .all()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        res.results::<T>().map_err(|e| anyhow::anyhow!("{e}"))
    }
}

#[derive(Deserialize)]
struct TokenRow {
    pk: String,
    #[serde(default)]
    spent: i64,
    #[serde(default)]
    data: String,
}
#[derive(Deserialize)]
struct CheckRow {
    pk: String,
    #[serde(default)]
    spent: i64,
}
#[derive(Deserialize)]
struct DataRow {
    #[serde(default)]
    data: String,
}

#[async_trait]
impl<A: Asset, K: KeyStrategy> LedgerStore<A> for D1Store<A, K>
where
    A::Record: HashRecord,
{
    async fn insert_tokens(&self, records: &[A::Record]) -> anyhow::Result<()> {
        SendFuture::new(async move {
            if records.is_empty() {
                return Ok(());
            }
            let mut stmts = Vec::with_capacity(records.len());
            for r in records {
                let pk = self.keys.token_key(A::NAME, &r.namespace(), r.public_hash());
                let data = record_to_data_json(r);
                stmts.push((INSERT_TOKEN, vec![vs(&pk), vi(r.amount_wats()), vs(&data)]));
            }
            self.run_batch(stmts).await.map_err(|e| anyhow::anyhow!(e))
        })
        .await
    }

    async fn get_tokens(
        &self,
        ns: &Namespace,
        hashes: &[String],
    ) -> anyhow::Result<Vec<Option<A::Record>>> {
        SendFuture::new(async move {
            if hashes.is_empty() {
                return Ok(vec![]);
            }
            let pks: Vec<String> =
                hashes.iter().map(|h| self.keys.token_key(A::NAME, ns, h)).collect();
            let mut by_pk: HashMap<String, (bool, String)> = HashMap::with_capacity(pks.len());
            for chunk in pks.chunks(READ_CHUNK) {
                let ph = (1..=chunk.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
                let sql = format!("SELECT pk, spent, data FROM tokens WHERE pk IN ({ph})");
                let params: Vec<JsValue> = chunk.iter().map(|s| vs(s)).collect();
                for row in self.query::<TokenRow>(&sql, params).await? {
                    by_pk.insert(row.pk, (row.spent != 0, row.data));
                }
            }
            Ok(pks
                .iter()
                .zip(hashes.iter())
                .map(|(pk, h)| by_pk.get(pk).and_then(|(sp, d)| data_json_to_record::<A>(h, *sp, d)))
                .collect())
        })
        .await
    }

    async fn check_tokens(
        &self,
        ns: &Namespace,
        hashes: &[String],
    ) -> anyhow::Result<Vec<(String, Option<bool>)>> {
        SendFuture::new(async move {
            if hashes.is_empty() {
                return Ok(vec![]);
            }
            let pks: Vec<String> =
                hashes.iter().map(|h| self.keys.token_key(A::NAME, ns, h)).collect();
            let mut by_pk: HashMap<String, bool> = HashMap::with_capacity(pks.len());
            for chunk in pks.chunks(READ_CHUNK) {
                let ph = (1..=chunk.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
                let sql = format!("SELECT pk, spent FROM tokens WHERE pk IN ({ph})");
                let params: Vec<JsValue> = chunk.iter().map(|s| vs(s)).collect();
                for row in self.query::<CheckRow>(&sql, params).await? {
                    by_pk.insert(row.pk, row.spent != 0);
                }
            }
            Ok(pks
                .into_iter()
                .zip(hashes.iter())
                .map(|(pk, h)| (h.clone(), by_pk.get(&pk).copied()))
                .collect())
        })
        .await
    }

    async fn batch_replace(&self, ns: &Namespace, ops: &[ReplaceOp<A::Record>]) -> Vec<ReplaceResult> {
        SendFuture::new(async move {
            let mut out = Vec::with_capacity(ops.len());
            for op in ops {
                let mut stmts: Vec<(&str, Vec<JsValue>)> = Vec::new();
                for h in &op.inputs {
                    let pk = self.keys.token_key(A::NAME, ns, h);
                    stmts.push((ASSERT_TOKEN_EXISTS, vec![vs(&pk)]));
                    stmts.push((UPDATE_SPENT, vec![vs(&pk)]));
                }
                for r in &op.outputs {
                    let pk = self.keys.token_key(A::NAME, ns, r.public_hash());
                    let data = record_to_data_json(r);
                    stmts.push((INSERT_TOKEN, vec![vs(&pk), vi(r.amount_wats()), vs(&data)]));
                }
                let audit_pk = self.keys.replacement_key(A::NAME, ns, &op.record.id);
                let audit = serde_json::to_string(&op.record).unwrap_or_else(|_| "{}".into());
                stmts.push((INSERT_AUDIT, vec![vs(&audit_pk), vs(&audit)]));
                out.push(match self.run_batch(stmts).await {
                    Ok(_) => ReplaceResult::Ok,
                    Err(e) => ReplaceResult::Failed(diagnostic_from_error(&e)),
                });
            }
            out
        })
        .await
    }

    async fn batch_burn(&self, ns: &Namespace, ops: &[(String, BurnRecord)]) -> anyhow::Result<()> {
        SendFuture::new(async move {
            for (hash, record) in ops {
                let pk = self.keys.token_key(A::NAME, ns, hash);
                let audit_pk = self.keys.burn_key(A::NAME, ns, &record.id);
                let audit = serde_json::to_string(record).unwrap_or_else(|_| "{}".into());
                self.run_batch(vec![
                    (ASSERT_TOKEN_EXISTS, vec![vs(&pk)]),
                    (UPDATE_SPENT, vec![vs(&pk)]),
                    (INSERT_AUDIT, vec![vs(&audit_pk), vs(&audit)]),
                ])
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            }
            Ok(())
        })
        .await
    }

    async fn get_mining_state(&self) -> anyhow::Result<Option<MiningState>> {
        SendFuture::new(async move {
            let pk = self.keys.mining_state_key(A::NAME);
            let rows = self.query::<DataRow>(SELECT_MINING_STATE, vec![vs(&pk)]).await?;
            Ok(rows.into_iter().next().and_then(|r| serde_json::from_str(&r.data).ok()))
        })
        .await
    }

    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()> {
        SendFuture::new(async move {
            let pk = self.keys.mining_state_key(A::NAME);
            let data = serde_json::to_string(state).unwrap_or_else(|_| "{}".into());
            self.run_batch(vec![(UPSERT_MINING_STATE, vec![vs(&pk), vs(&data)])])
                .await
                .map_err(|e| anyhow::anyhow!(e))
        })
        .await
    }
}

fn record_to_data_json<R: HashRecord>(record: &R) -> String {
    let mut fields: HashMap<String, String> = HashMap::new();
    record.to_fields(&mut fields);
    serde_json::to_string(&fields).unwrap_or_else(|_| "{}".into())
}

fn data_json_to_record<A: Asset>(public_hash: &str, spent: bool, data_json: &str) -> Option<A::Record>
where
    A::Record: HashRecord,
{
    let mut fields: HashMap<String, String> = serde_json::from_str(data_json).ok()?;
    fields.insert("spent".into(), if spent { "1" } else { "0" }.into());
    <A::Record as HashRecord>::from_fields(public_hash, &fields)
}

fn diagnostic_from_error(err: &str) -> String {
    if err.contains("double_spend") {
        "double_spend".into()
    } else if err.contains("input_missing") {
        "input_missing".into()
    } else if err.contains("constraint failed") {
        "output_collision".into()
    } else {
        err.to_string()
    }
}

// ── worker entry ──
#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let db = match env.d1("DB") {
        Ok(d) => d,
        Err(e) => return Response::error(format!("no D1 binding: {e}"), 500),
    };
    let proto = match to_proto(&mut req).await {
        Ok(p) => p,
        Err(e) => return Response::error(format!("request: {e}"), 400),
    };

    // Webcash flavor: flat keyspace, no issuers, splittable route.
    let store = D1Store::<Webcash, WebcashLegacyKeys>::new(db, WebcashLegacyKeys);
    let server = Server::<Webcash, _>::new(ServeConfig::testnet_default(), store);
    let resp = SendFuture::new(route_proto(&server, &proto)).await;
    to_worker(resp)
}

async fn to_proto(req: &mut Request) -> Result<ProtoRequest> {
    let method = req.method().to_string().to_uppercase();
    let url = req.url()?;
    let path = url.path().to_string();
    let mut headers = HashMap::new();
    for (k, v) in req.headers().entries() {
        headers.insert(k.to_lowercase(), v);
    }
    let body = req.bytes().await.unwrap_or_default();
    Ok(ProtoRequest { method, path, headers, body: bytes::Bytes::from(body) })
}

fn to_worker(resp: ProtoResponse) -> Result<Response> {
    let body = resp.body.to_vec();
    let mut out = Response::from_bytes(body)?.with_status(resp.status);
    let h = out.headers_mut();
    for (k, v) in &resp.headers {
        let _ = h.set(k, v);
    }
    Ok(out)
}
