[server]
mode = "${environment}"
bind_addr = "0.0.0.0:8080"

[server.db]
backend = "${db_backend}"
redis_url = "redis://${redis_host}:6379"
fdb_cluster_file = "${fdb_cluster_file}"

[mining]
testnet_difficulty = ${mining_difficulty}
initial_difficulty = ${mining_difficulty}
reports_per_epoch = ${environment == "production" ? 1000 : 100}
target_epoch_seconds = ${environment == "production" ? 10000 : 1000}
initial_mining_amount_wats = ${mining_amount_wats}
initial_subsidy_amount_wats = 0
