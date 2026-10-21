"""Splits omlx's requirements for an install over the fork's MLX.

    python3 omlx_reqs.py <omlx pyproject.toml> <plain.txt> <pinned.txt>

writes to plain.txt the requirements pip may resolve as usual, and to pinned.txt the git-pinned
MLX packages (mlx-lm, mlx-vlm, mlx-embeddings, mlx-audio), which go in with --no-deps so they
cannot pull MLX from PyPI. MLX itself is left out: the fork's is already installed.
"""

import sys
import tomllib

project = tomllib.load(open(sys.argv[1], "rb"))["project"]
plain, pinned = [], []
for r in project["dependencies"]:
    name = r.split("@")[0].split("[")[0].split("=")[0].split("<")[0].split(">")[0].strip().lower()
    if name == "mlx":
        continue
    (pinned if name.startswith("mlx-") else plain).append(r)
open(sys.argv[2], "w").write("\n".join(plain) + "\n")
open(sys.argv[3], "w").write("\n".join(pinned) + "\n")
print(f"{len(plain)} plain, {len(pinned)} pinned: {', '.join(p.split(' ')[0] for p in pinned)}")
