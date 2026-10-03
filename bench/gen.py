import os, sys, random
root = sys.argv[1]
n = int(sys.argv[2]) if len(sys.argv) > 2 else 100_000
os.makedirs(root, exist_ok=True)
exts = ["txt","jpg","png","rs","py","log","md","json","bin","mp3"]
random.seed(1)
for i in range(n):
    name = f"file_{i:07d}.{random.choice(exts)}"
    with open(os.path.join(root, name), "wb") as f:
        f.write(os.urandom(random.randint(0, 200)))
print("done", n)
