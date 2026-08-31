#!/usr/bin/env python3
"""Turn the raw GitHub material into the compact JSONL the corpus builder reads.

Two files are produced:

  code.jsonl    one record per source file, carrying the repository, the path and
                the file text. Real code is what gives the corpus its file paths,
                function names and compound identifiers, which one graded
                scenario searches for as rare literal strings.
  issues.jsonl  one record per issue, carrying the repository, number, title and
                body. Real issue threads give the ticket shaped source its
                register and its cross references.

The issue files are read tolerantly. `gh api --paginate` concatenates one JSON
array per page, and a fetch stopped part way leaves the final array unterminated,
so every complete object before that point is still usable.
"""
import json
import sys
from pathlib import Path

# Extensions kept per repository, chosen so each repository contributes the
# language it is actually written in.
CODE_EXTENSIONS = {".rs", ".py", ".ts", ".go", ".js", ".pyx", ".tsx", ".php", ".jsx", ".mjs"}
SKIP_PARTS = {".git", "node_modules", "target", "dist", "vendor", "testdata", "__pycache__"}
MIN_CODE_CHARS = 1500
MIN_ISSUE_CHARS = 300


def read_objects(path):
    """Every complete JSON object in a file of concatenated arrays."""
    text = Path(path).read_text(encoding="utf-8", errors="replace")
    decoder = json.JSONDecoder()
    out = []
    i = 0
    n = len(text)
    while i < n:
        while i < n and text[i] in " \n\r\t,[]":
            i += 1
        if i >= n:
            break
        try:
            obj, end = decoder.raw_decode(text, i)
        except json.JSONDecodeError:
            # The truncated tail of an interrupted fetch.
            break
        out.append(obj)
        i = end
    return out


def write_code(repos_dir, out_path):
    kept = 0
    with open(out_path, "w", encoding="utf-8") as fh:
        for repo_dir in sorted(Path(repos_dir).iterdir()):
            if not repo_dir.is_dir():
                continue
            repo = repo_dir.name
            files = []
            for path in sorted(repo_dir.rglob("*")):
                if not path.is_file():
                    continue
                if any(part in SKIP_PARTS for part in path.parts):
                    continue
                if path.suffix not in CODE_EXTENSIONS:
                    continue
                try:
                    text = path.read_text(encoding="utf-8")
                except (UnicodeDecodeError, OSError):
                    continue
                if len(text) < MIN_CODE_CHARS:
                    continue
                files.append((str(path.relative_to(repo_dir)), text))
            for rel, text in files:
                fh.write(json.dumps({"repo": repo, "path": rel, "text": text}, ensure_ascii=False) + "\n")
                kept += 1
            print(f"  {repo}: {len(files)} source files", flush=True)
    return kept


def write_issues(raw_dir, out_path):
    kept = 0
    with open(out_path, "w", encoding="utf-8") as fh:
        for path in sorted(Path(raw_dir).glob("issues-*.json")):
            repo = path.stem.replace("issues-", "")
            items = read_objects(path)
            n = 0
            for it in items:
                if "pull_request" in it:
                    continue
                body = it.get("body") or ""
                title = (it.get("title") or "").strip()
                if len(body) < MIN_ISSUE_CHARS or not title:
                    continue
                fh.write(json.dumps({
                    "repo": repo,
                    "number": it.get("number"),
                    "title": title,
                    "body": body,
                    "labels": [l.get("name") for l in (it.get("labels") or []) if isinstance(l, dict) and l.get("name")],
                    "created_at": it.get("created_at"),
                    "updated_at": it.get("updated_at"),
                }, ensure_ascii=False) + "\n")
                n += 1
                kept += 1
            print(f"  {repo}: {len(items)} fetched, {n} issues with a body of {MIN_ISSUE_CHARS}+ characters", flush=True)
    return kept


def main():
    raw = Path(sys.argv[1])
    out = Path(sys.argv[2])
    out.mkdir(parents=True, exist_ok=True)
    print("code.jsonl:", flush=True)
    code = write_code(raw / "repos", out / "code.jsonl")
    print(f"code.jsonl: kept {code} source files", flush=True)
    print("issues.jsonl:", flush=True)
    issues = write_issues(raw, out / "issues.jsonl")
    print(f"issues.jsonl: kept {issues} issues", flush=True)


if __name__ == "__main__":
    main()
