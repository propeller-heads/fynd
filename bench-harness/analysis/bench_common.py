"""Helpers the benchmark analysis scripts share.

Underscored so the hyphenated scripts beside it can import it: Python puts a script's own
directory first on `sys.path`.
"""

import json


def load_jsonl(path):
    """Yields one parsed object per non-empty line of a JSON Lines file."""
    with open(path) as handle:
        for line in handle:
            line = line.strip()
            if line:
                yield json.loads(line)
