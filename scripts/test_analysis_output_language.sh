#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PYTHON_BIN="${ASTRAL_PYTHON:-python3}"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

cat > "$WORK_DIR/fixture.json" <<'JSON'
[
  {
    "vector": [0.0, 0.0, 0.0, 0.0],
    "function_name": "withdraw",
    "contract_name": "Vault",
    "file_path": "contracts/Vault.sol",
    "code_content": "function withdraw() external { uint256 amount = balances[msg.sender]; (bool ok, ) = msg.sender.call{value: amount}(\"\"); require(ok); balances[msg.sender] = 0; }",
    "referenced_symbols": ["balances", "msg.sender.call{value: amount}", "require"],
    "risk_score": 7,
    "kind": "function",
    "start_line": 10,
    "end_line": 14
  },
  {
    "vector": [0.1, 0.0, 0.0, 0.0],
    "function_name": "deposit",
    "contract_name": "Vault",
    "file_path": "contracts/Vault.sol",
    "code_content": "function deposit() external payable { balances[msg.sender] += msg.value; }",
    "referenced_symbols": ["balances", "msg.value"],
    "risk_score": 3,
    "kind": "function",
    "start_line": 5,
    "end_line": 7
  },
  {
    "vector": [5.0, 5.0, 5.0, 5.0],
    "function_name": "hashOrder",
    "contract_name": "Orders",
    "file_path": "contracts/Orders.sol",
    "code_content": "function hashOrder(bytes32 salt) internal view returns (bytes32) { return keccak256(abi.encode(salt, address(this))); }",
    "referenced_symbols": ["abi.encode", "address"],
    "risk_score": 0,
    "kind": "function",
    "start_line": 20,
    "end_line": 22
  }
]
JSON

"$PYTHON_BIN" "$ROOT_DIR/analysis/semantic_risk_analysis.py" \
  --input "$WORK_DIR/missing.parquet" \
  --lance-json "$WORK_DIR/fixture.json" \
  --out-dir "$WORK_DIR/out" \
  > "$WORK_DIR/stdout.json"

if "$PYTHON_BIN" - "$WORK_DIR/out/semantic_risk_report.json" "$WORK_DIR/out/semantic_risk_report.md" "$WORK_DIR/stdout.json" <<'PY'
import sys
from pathlib import Path

def has_cyrillic(text: str) -> bool:
    return any(
        0x0400 <= ord(char) <= 0x052F
        or 0x2DE0 <= ord(char) <= 0x2DFF
        or 0xA640 <= ord(char) <= 0xA69F
        for char in text
    )

for raw_path in sys.argv[1:]:
    path = Path(raw_path)
    text = path.read_text(errors="replace")
    for line_no, line in enumerate(text.splitlines(), 1):
        if has_cyrillic(line):
            print(f"{path}:{line_no}:{line}")
            sys.exit(0)
sys.exit(1)
PY
then
  echo "analysis output must be English-only; found Cyrillic text" >&2
  exit 1
fi

echo "analysis output language test passed"
