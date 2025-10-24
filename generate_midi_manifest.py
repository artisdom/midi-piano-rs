import os, json
base = "assets/midi"
paths = []
for root, dirs, files in os.walk(base):
    dirs.sort()
    files.sort()
    for f in files:
        if f.lower().endswith(('.mid', '.midi')):
            rel = os.path.relpath(os.path.join(root, f), base)
            paths.append(rel.replace(os.sep, '/'))
paths.sort()
with open('assets/midi_manifest.json', 'w', encoding='utf-8') as fh:
    json.dump(paths, fh, ensure_ascii=False, indent=2)

print(f"Generated manifest with {len(paths)} MIDI files.")
# This script generates a JSON manifest of all MIDI files in the assets/midi directory.