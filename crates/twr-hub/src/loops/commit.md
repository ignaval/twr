Then commit {{what}}: one commit per repository, in that repository's own message style (read
its `git log` for the subject form, body, trailers and sign-offs it uses). {{baseline}}Do not
amend or rewrite earlier commits, and do not push. Afterwards nothing of the loop's work may be
left uncommitted: the hub checks each repository ({{repos}}) with git: the loop's work is in a
new commit (HEAD moved, to a commit that changes something), nothing is left in its worktree
beyond what was there when the loop began, and what was there then is still uncommitted, as it
was.

End your final message with one fenced block, info string `twr-commit`, and nothing after it:
the commits you made, like this (without the indentation):

    ```twr-commit
    {"commits": [{"repo": "/path/to/repo", "sha": "<full or short sha>"}]}
    ```

The block is your note of what you committed: the hub checks git itself, not the block. The
text above it is your report.
