#!/bin/bash
set -euxo pipefail

source /opt/miniconda3/bin/activate
conda activate testbed
cd /testbed

git config --global --add safe.directory /testbed
cat > /tmp/test_vellum_swe_regression.py <<'PY'
import sys

sys.path.insert(0, "/testbed")
import requests


def test_no_content_length_for_bodyless_get_and_head():
    get_request = requests.Request("GET", "http://example.invalid/get").prepare()
    assert "Content-Length" not in get_request.headers
    head_request = requests.Request("HEAD", "http://example.invalid/head").prepare()
    assert "Content-Length" not in head_request.headers
PY

# This historical Requests fixture has unrelated Python 3.11 failures. The
# pinned SWE-bench regression is the authoritative acceptance check here.
pytest -q /tmp/test_vellum_swe_regression.py
