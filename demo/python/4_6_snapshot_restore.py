from pathlib import Path

from common import DemoError, build_parser, client_from_args, require_directory


def main():
    parser = build_parser("Section 4.6: snapshot save and manifest verification")
    parser.add_argument("--snapshot-root", default="snapshot", help="local snapshot directory of the demo service")
    args = parser.parse_args()
    client = client_from_args(args)
    client.sql("CREATE TABLE IF NOT EXISTS demo_4_6_snapshot_probe(id INTEGER, created_at TIMESTAMP)")
    client.sql("INSERT INTO demo_4_6_snapshot_probe VALUES (1, now())")
    result = client.snapshot()
    print(result["msg"])
    root = require_directory(args.snapshot_root)
    manifests = sorted(root.glob("*/manifest.json"), key=lambda path: path.stat().st_mtime)
    if not manifests:
        raise DemoError("snapshot endpoint succeeded but no manifest.json was found under %s" % root)
    print("verified snapshot manifest: %s" % manifests[-1])
    print("restore is validated by restarting an isolated rsduck service with this snapshot directory")
    if args.cleanup:
        client.sql("DROP TABLE IF EXISTS demo_4_6_snapshot_probe")


if __name__ == "__main__":
    main()
