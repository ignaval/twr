#!/bin/sh
# Applies the repository settings the workflows rely on: a tag ruleset for v*, branch
# protection on main and dev with CI's checks required, and a required reviewer on the
# `release` environment. Safe to run again: it replaces each setting.
#
#   docs/tools/github-settings.sh [owner/repo]
#
# Needs the GitHub CLI, signed in as a repository admin. Set in the repository's settings
# instead (docs/development.md, "Repository settings"): actions must be pinned to a full commit
# SHA, the `release` environment deploys only from `v*` tags and `main`, and the security
# features.

set -eu

repo=${1:-ignaval/twr}
me=$(gh api user --jq .id)

# Only admins may create, move or delete a v* tag (a pushed v* tag starts a release).
ruleset=$(gh api "repos/$repo/rulesets" --jq '.[] | select(.name == "release tags") | .id')
body=$(cat << 'EOF'
{
  "name": "release tags",
  "target": "tag",
  "enforcement": "active",
  "conditions": { "ref_name": { "include": ["refs/tags/v*"], "exclude": [] } },
  "rules": [{ "type": "creation" }, { "type": "update" }, { "type": "deletion" }],
  "bypass_actors": [{ "actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "always" }]
}
EOF
)
if [ -n "$ruleset" ]; then
    printf '%s' "$body" | gh api -X PUT "repos/$repo/rulesets/$ruleset" --input - > /dev/null
else
    printf '%s' "$body" | gh api -X POST "repos/$repo/rulesets" --input - > /dev/null
fi
echo "tag ruleset: v* tags are admin-only"

# main and dev: changes from others go through a pull request whose CI jobs passed; no force
# pushes or deletion. The checks are the `name:` of each job ci.yml runs on a pull request,
# one per matrix entry, reported by GitHub Actions (app 15368) only; the release builds run by
# hand and are not required. Admins are not bound (enforce_admins false): the maintainer
# pushes to dev and fast-forwards main for a release (docs/development.md).
for branch in main dev; do
    gh api -X PUT "repos/$repo/branches/$branch/protection" --input - > /dev/null << 'EOF'
{
  "required_status_checks": {
    "strict": false,
    "checks": [
      { "context": "fmt + shellcheck + doc links", "app_id": 15368 },
      { "context": "clippy + test (ubuntu-latest)", "app_id": 15368 },
      { "context": "clippy + test (macos-latest)", "app_id": 15368 },
      { "context": "reap tests with the ps backend (linux)", "app_id": 15368 },
      { "context": "cargo deny", "app_id": 15368 },
      { "context": "actionlint", "app_id": 15368 },
      { "context": "real systemd (linux)", "app_id": 15368 }
    ]
  },
  "enforce_admins": false,
  "required_pull_request_reviews": { "required_approving_review_count": 0 },
  "restrictions": null,
  "allow_force_pushes": false,
  "allow_deletions": false
}
EOF
    echo "$branch: pull requests with CI passed, no force pushes or deletion (admins exempt)"
done

# Publishing a release waits for the owner's approval; it still deploys only from v* tags
# and main (the deployment policies stay as they are).
gh api -X PUT "repos/$repo/environments/release" --input - > /dev/null << EOF
{
  "prevent_self_review": false,
  "reviewers": [{ "type": "User", "id": $me }],
  "deployment_branch_policy": { "protected_branches": false, "custom_branch_policies": true }
}
EOF
echo "release environment: publishing waits for your approval"
