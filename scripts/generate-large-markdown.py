"""Generate the ignored large-preview visual fixture; no binary test data in Git."""
import argparse
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('path', type=Path)
parser.add_argument('--mib', type=int, default=50)
args = parser.parse_args()
args.path.parent.mkdir(parents=True, exist_ok=True)
limit = args.mib * 1024 * 1024
written = count = 0
with args.path.open('wb') as output:
    while True:
        block = (
            f'# Section {count:06}\n\n'
            f'Preview block {count:06} has **bold text**, a [link](https://example.com) and `code`.\n\n'
            f'```rust\nlet section = {count:06};\n```\n\n'
        ).encode()
        if written + len(block) > limit:
            break
        output.write(block)
        written += len(block)
        count += 1
print(f'path={args.path.as_posix()} bytes={written} blocks={count} middle_line={count // 2 * 8} middle_section={count // 2:06}')
