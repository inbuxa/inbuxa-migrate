#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
# SPDX-License-Identifier: Apache-2.0 OR MIT
#
# Take an upstream release in: commit it, renamed, onto the `upstream` branch,
# ready to merge into main.
#
# Usage: scripts/upstream-import.py REF
#        scripts/upstream-import.py v1.0.13
#        scripts/upstream-import.py --first 486a384
#
# REF is an upstream tag or commit. The tag is fetched from the fetch-only
# `upstream` remote into refs/upstream-tags/, never refs/tags/: a pushed
# upstream tag would start a release here.
#
# The snapshot is upstream's tree with the fork's rename applied (the crate,
# binary, variables and test names, as in "Rename to inbuxa-migrate"), the
# fork's own files in place of upstream's (OWNED) and upstream's release
# tooling left out (DROPPED). Its parent is the branch's last snapshot, so
# merging the branch into main brings in only what upstream changed between
# the two releases, already renamed. Nothing in the working tree is touched.
#
# --first starts the branch. Give it the upstream commit the fork was made
# from; the snapshot's parent is that commit. Merge it into main once with
# `git merge -s ours upstream`, which records that main already has it, and
# later imports merge cleanly against it.
#
# After an import:
#   git switch -c import/upstream-REF origin/main
#   git merge upstream
#   (resolve; for Cargo.lock take main's and let `cargo build` add what's new)
# then the usual checks and a PR. Push `upstream` to origin only once that PR
# has landed. The upstream-watch workflow reads the imported release from the
# branch's head subject.

import argparse
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile

REMOTE = "upstream"
BRANCH = "upstream"
REPO_URL = "https://git.coffeylabs.org/inbuxa/inbuxa-migrate"

# The fork's own copies always win: upstream's versions of these are dropped
# and main's are put in their place.
OWNED = ["README.md", "CHANGELOG.md", ".gitignore", ".github", ".gitea"]
# Upstream's release tooling, which the fork replaced.
DROPPED = ["dist-workspace.toml", "wix", "assets/importer-exporter.jpg"]

# Order matters: the specific forms before the plain name.
RENAMES = [
    (re.compile(r"https://github\.com/stalwartlabs/vandelay"), REPO_URL),
    (re.compile(r"\bvandelay(?=::|_)"), "inbuxa_migrate"),
    (re.compile(r"\b(use|extern crate) vandelay\b"), r"\1 inbuxa_migrate"),
    (re.compile(r"VANDELAY_"), "INBUXA_MIGRATE_"),
    (re.compile(r"VANDELAY"), "INBUXA"),
    (re.compile(r"\ba Vandelay\b"), "an inbuxa-migrate"),
    (re.compile(r"Vandelay"), "Inbuxa"),
    (re.compile(r"vandelay"), "inbuxa-migrate"),
]
OLD_NAME = re.compile(r"vandelay", re.I)


def git(*args, capture=True, env=None, check=True):
    r = subprocess.run(["git", *args], capture_output=capture, text=True, env=env)
    if check and r.returncode != 0:
        sys.exit(f"git {' '.join(args)}: {r.stderr.strip()}")
    return r.stdout.strip() if capture else ""


def extract(rev, dest, paths=()):
    """Write the tree of `rev` (or only `paths` in it) into `dest`."""
    present = [p for p in paths
               if subprocess.run(["git", "cat-file", "-e", f"{rev}:{p}"], capture_output=True).returncode == 0]
    if paths and not present:
        return
    archive = subprocess.run(["git", "archive", "--format=tar", rev, *present],
                             capture_output=True, check=True).stdout
    with tempfile.TemporaryFile() as f:
        f.write(archive)
        f.seek(0)
        with tarfile.open(fileobj=f) as tar:
            tar.extractall(dest, filter="tar")


def remove(root, rel):
    path = os.path.join(root, rel)
    if os.path.isdir(path) and not os.path.islink(path):
        shutil.rmtree(path)
    elif os.path.lexists(path):
        os.remove(path)


def rename_text(text):
    for pattern, new in RENAMES:
        text = pattern.sub(new, text)
    return text


def fork_cargo_toml(upstream_text, fork_text):
    """Upstream's Cargo.toml with the fork's [package] table, and without the
    tables the fork's release tooling dropped."""
    head = fork_text[: fork_text.index("[[bin]]")]
    body = upstream_text[upstream_text.index("[[bin]]"):]
    for table in ("[profile.dist]", "[workspace.metadata.dist]"):
        body = re.sub(r"(?ms)^" + re.escape(table) + r"\n.*?(?=^\[|\Z)", "", body)
    return head + body.rstrip("\n") + "\n"


def apply(root, fork):
    """Rename in place; returns the files the rename changed and the paths
    still naming the old project afterwards."""
    changed, left = [], []
    for dirpath, dirnames, filenames in os.walk(root):
        for name in filenames:
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, root)
            if os.path.islink(path) or any(rel == o or rel.startswith(o + "/") for o in OWNED):
                continue
            try:
                with open(path, encoding="utf-8", newline="") as f:
                    text = f.read()
            except UnicodeDecodeError:
                continue
            new = rename_text(text)
            if rel == "Cargo.toml":
                new = fork_cargo_toml(new, git("show", f"{fork}:Cargo.toml"))
            if new != text:
                with open(path, "w", encoding="utf-8", newline="") as f:
                    f.write(new)
                changed.append(rel)
            if OLD_NAME.search(new):
                left.append(rel)
    for dirpath, dirnames, filenames in os.walk(root, topdown=False):
        for name in filenames + dirnames:
            if OLD_NAME.search(name):
                new = rename_text(name)
                os.rename(os.path.join(dirpath, name), os.path.join(dirpath, new))
                changed.append(os.path.relpath(os.path.join(dirpath, new), root))
    return sorted(changed), sorted(left)


def main():
    ap = argparse.ArgumentParser(description="Commit an upstream release, renamed, onto the upstream branch.")
    ap.add_argument("ref", help="upstream tag (v1.0.13) or commit")
    ap.add_argument("--first", action="store_true", help="start the branch at this upstream commit")
    ap.add_argument("--fork", default="origin/main", help="where the fork's own files come from (origin/main)")
    args = ap.parse_args()

    os.chdir(git("rev-parse", "--show-toplevel"))
    if git("remote", "get-url", "--push", REMOTE, check=False) not in ("DISABLED", ""):
        sys.exit(f"the `{REMOTE}` remote must be fetch-only (push URL DISABLED)")

    if re.fullmatch(r"v\d+\.\d+\.\d+", args.ref):
        git("fetch", "--no-tags", REMOTE, f"+refs/tags/{args.ref}:refs/upstream-tags/{args.ref}")
        rev = git("rev-parse", f"refs/upstream-tags/{args.ref}^{{commit}}")
    else:
        git("fetch", "--no-tags", REMOTE, "+refs/tags/*:refs/upstream-tags/*")
        rev = git("rev-parse", f"{args.ref}^{{commit}}")
    label = args.ref if args.ref.startswith("v") else rev[:7]

    have = git("rev-parse", "--verify", "--quiet", f"refs/heads/{BRANCH}", check=False)
    if args.first == bool(have):
        sys.exit(f"`{BRANCH}` already exists; leave out --first" if have
                 else f"no `{BRANCH}` branch yet; start it with --first")
    parent = rev if args.first else have

    with tempfile.TemporaryDirectory(prefix="upstream-import-") as tmp:
        tree_dir = os.path.join(tmp, "tree")
        os.mkdir(tree_dir)
        extract(rev, tree_dir)
        for rel in OWNED + DROPPED:
            remove(tree_dir, rel)
        extract(args.fork, tree_dir, OWNED)
        changed, left = apply(tree_dir, args.fork)
        # The rename moves imports and line breaks; main is kept formatted.
        rust = [os.path.join(tree_dir, rel) for rel in changed if rel.endswith(".rs")]
        if rust and subprocess.run(["rustfmt", "--edition", "2024", *rust]).returncode != 0:
            sys.exit("rustfmt failed on the renamed files")

        env = dict(os.environ, GIT_INDEX_FILE=os.path.join(tmp, "index"))
        git("--work-tree", tree_dir, "add", "--all", "--force", ".", env=env)
        tree = git("write-tree", env=env)

    if not args.first and git("rev-parse", f"{parent}^{{tree}}") == tree:
        print(f"{label}: nothing changed since the last import.")
        return
    if args.first:
        # The newest release the commit includes, which the watch counts from.
        tags = git("for-each-ref", "--merged", rev, "--sort=-version:refname",
                   "--format=%(refname:lstrip=2)", "refs/upstream-tags/v*").split()
        label = f"{label} ({tags[0] if tags else 'untagged'} and later)"
    message = (f"Import upstream {label}, renamed\n\n"
               f"Upstream commit {rev}, renamed by scripts/upstream-import.py.\n")
    commit = git("commit-tree", "-S", tree, "-p", parent, "-m", message)
    git("update-ref", f"refs/heads/{BRANCH}", commit, *([] if args.first else [parent]))

    print(f"{BRANCH} is now {commit[:7]}: upstream {label}, renamed.")
    print(f"The rename changed {len(changed)} file(s).")
    if left:
        print("Still naming the old project, to look at before merging:")
        for rel in left:
            print(f"  {rel}")


if __name__ == "__main__":
    main()
