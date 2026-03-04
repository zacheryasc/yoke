#!/usr/bin/env bash
# Scratch setup script for visual testing
set -euo pipefail

echo "Setting up scratch environment..."

PROJECT_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SCRATCH_DIR="${PROJECT_ROOT}/src/scratch"

echo "Project root: ${PROJECT_ROOT}"
echo "Scratch dir: ${SCRATCH_DIR}"

# Count files
FILE_COUNT=$(find "${SCRATCH_DIR}" -type f | wc -l)
echo "Found ${FILE_COUNT} scratch files"

# List them
for f in "${SCRATCH_DIR}"/*; do
    if [ -f "$f" ]; then
        echo "  - $(basename "$f") ($(wc -l < "$f") lines)"
    fi
done

echo "Done."
