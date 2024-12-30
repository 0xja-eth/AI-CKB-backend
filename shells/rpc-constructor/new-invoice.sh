#!/bin/bash

# Check if amount is provided
if [ -z "$1" ]; then
    echo "Usage: $0 <amount_in_ckb> [description]"
    exit 1
fi

# Get amount and description
amount_in_ckb=$1
description=${2:-"Test invoice"}

# Convert amount to shannon (CKB * 10^8) and then to hex
amount_in_shannon=$(echo "$amount_in_ckb * 100000000" | bc)
amount_hex="0x$(printf "%x" $amount_in_shannon)"

# Generate random 32-byte hex string for payment_preimage
payment_preimage="0x$(openssl rand -hex 32)"

# Get current timestamp for id
id=$(date +%s)

# Construct JSON payload
json_payload=$(cat << EOF
{
    "jsonrpc": "2.0",
    "method": "new_invoice",
    "params": [
        {
            "amount": "$amount_hex",
            "currency": "fibt",
            "description": "$description",
            "expiry": "0xe10",
            "payment_preimage": "$payment_preimage",
            "hash_algorithm": "sha256"
        }
    ],
    "id": $id
}
EOF
)

# Print the curl command
echo "curl -X POST http://127.0.0.1:8227 \\"
echo "  -H 'Content-Type: application/json' \\"
echo "  -d '$json_payload'"
