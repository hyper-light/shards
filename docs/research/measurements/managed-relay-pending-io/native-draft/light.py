#!/usr/bin/env python3
"""Compile ignored native witnesses; run only the existing default pure suite."""
from pathlib import Path
source = Path('docs/research/measurements/managed-network-sock-addr/light.py').read_text().replace("'-j4'", "'-j1'")
exec(compile(source, 'managed-network-sock-addr/light.py', 'exec'))
