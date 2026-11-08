from pathlib import Path

root = Path(__file__).resolve().parent / "fixtures"
data = root / "data"
tree = root / "tree"
data.mkdir(parents=True, exist_ok=True)
tree.mkdir(parents=True, exist_ok=True)

for i in range(32):
    lines = [f"NEEDLE_{i:02d}"]
    lines += [f"line {n:03d} " + ("x" * 40) for n in range(200)]
    (data / f"read_{i:02d}.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")

for i in range(512):
    (tree / f"file_{i:04d}.txt").write_text(
        f"needle_{i:04d}\nTODO(high): item {i:04d}\nplain line {i:04d}\n",
        encoding="utf-8",
    )

print(f"read files: {len(list(data.glob('read_*.txt')))}")
print(f"tree files: {len(list(tree.glob('*.txt')))}")
