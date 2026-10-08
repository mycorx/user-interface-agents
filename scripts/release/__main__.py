import sys
from pathlib import Path

# Run as `python3 scripts/release ...`, this directory is sys.path[0], not its
# parent, so the package would not be importable by name. Put `scripts/` on the
# path and import it as the package it is.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from release import cli  # noqa: E402

if __name__ == "__main__":
    sys.exit(cli.main(sys.argv[1:]))
