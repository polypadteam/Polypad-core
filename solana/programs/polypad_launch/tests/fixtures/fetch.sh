#!/usr/bin/env bash
# Dump the real Meteora programs (and the Metaplex program DBC calls) from
# mainnet for the LiteSVM tests. The binaries are ~5 MB together and change
# whenever Meteora upgrades, so they are fetched, not committed; the small
# DAMM v2 config accounts (`Hv8L…`, and two other DBC-only configs the
# tests try to migrate through) are committed and refreshed here.
#
# Usage: solana/programs/polypad_launch/tests/fixtures/fetch.sh [rpc-url]
# Prints the sha256 of each binary against the ones the tests were last run
# with, so a Meteora upgrade shows up before a confusing test failure.
set -euo pipefail

RPC="${1:-https://api.mainnet-beta.solana.com}"
DIR="$(cd "$(dirname "$0")" && pwd)"

DBC=dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN
DAMM=cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG
METAPLEX=metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s
HV8L=Hv8Lmzmnju6m7kcokVKvwqz7QPmdX9XfKjJsXz8RXcjp
# DBC-only DAMM v2 configs at 2% (static) and the customizable one.
TWO_PCT=2c4cYd4reUYVRAB9kUUkrq55VPyy2FNQ3FDL4o12JXmq
CUSTOM=A8gMrEPJkacWkcb3DGwtJwTe16HktSEfvwtuDh2MCtck
DBC_POOL_AUTHORITY=FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM

solana program dump --url "$RPC" "$DBC" "$DIR/dbc.so"
solana program dump --url "$RPC" "$DAMM" "$DIR/cp_amm.so"
solana program dump --url "$RPC" "$METAPLEX" "$DIR/mpl_token_metadata.so"
solana account --url "$RPC" "$HV8L" --output json-compact --output-file "$DIR/hv8l.json" >/dev/null
solana account --url "$RPC" "$TWO_PCT" --output json-compact --output-file "$DIR/damm_config_2pct.json" >/dev/null
solana account --url "$RPC" "$CUSTOM" --output json-compact --output-file "$DIR/damm_config_custom.json" >/dev/null

# Graduation is safe from a front-run migration into another pool only
# while `Hv8L…` is the one DBC-only static DAMM v2 config with a flat 1%
# fee (DBC accepts no other for our migration fee option; METEORA.md §4).
# List every config only DBC may use (cp-amm Config: 328 bytes, the pool
# creator authority at offset 40) and compare with the set seen on
# 2026-10-13 (one per fee level 0.25/0.3/1/2/4/6%, plus A8gM).
curl -s "$RPC" -X POST -H 'content-type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"getProgramAccounts","params":["'$DAMM'",{"encoding":"base64","dataSlice":{"offset":0,"length":0},"filters":[{"dataSize":328},{"memcmp":{"offset":40,"bytes":"'$DBC_POOL_AUTHORITY'"}}]}]}' \
  | python3 -I -c 'import json,sys; print("\n".join(sorted(a["pubkey"] for a in json.load(sys.stdin)["result"])))' > "$DIR/dbc_damm_configs.txt"
cat > "$DIR/expected_configs.txt" <<'EOF'
2c4cYd4reUYVRAB9kUUkrq55VPyy2FNQ3FDL4o12JXmq
2nHK1kju6XjphBLbNxpM5XRGFj7p9U8vvNzyZiha1z6k
7F6dnUcRuyM2TwR8myT1dYypFXpPSxqwKNSFNkxyNESd
A8gMrEPJkacWkcb3DGwtJwTe16HktSEfvwtuDh2MCtck
AkmQWebAwFvWk55wBoCr5D62C6VVDTzi84NJuD9H7cFD
DbCRBj8McvPYHJG1ukj8RE15h2dCNUdTAESG49XpQ44u
Hv8Lmzmnju6m7kcokVKvwqz7QPmdX9XfKjJsXz8RXcjp
EOF
diff "$DIR/expected_configs.txt" "$DIR/dbc_damm_configs.txt" >/dev/null \
  || echo "warning: the DBC-only DAMM v2 configs changed; if a new one has a flat 1% fee, a coin could be migrated into it first (see METEORA.md §4)"
rm -f "$DIR/expected_configs.txt" "$DIR/dbc_damm_configs.txt"

# Tested against (2026-10-13): DBC deployed slot 445503633 (0.2.1),
# cp-amm slot 445230614 (0.2.4).
cat > "$DIR/expected.sha256" <<'EOF'
4c26a8a5da99f8ce932fa0300c46675b527090021fbb74214c9486bedda9f23b  dbc.so
4d5b920baebc090f89b2e8796a3452ed067c9667a143058c96a312f2c1e6848b  cp_amm.so
31f0a627dba051a938de650464e55cc5397a4be0fd496929c1f9cf02fe5e9011  mpl_token_metadata.so
EOF
(cd "$DIR" && shasum -a 256 -c expected.sha256) || echo "warning: mainnet binaries differ from the tested ones; re-run the tests and re-diff the IDLs"
rm -f "$DIR/expected.sha256"
