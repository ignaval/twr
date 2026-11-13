The plan is settled: {{plan_path}}{{plan_note}}. Now implement it, across every repository it
names. Never pause for the user: decide from the code, the plan and sensible defaults.

- Decompose the plan into work items, roughly one per step or cluster of files.
{{workers}}
- Review every change, yours or a subagent's, against the plan and the code around it before
  you count it done; fix it or hand it back with specific feedback. Nothing is done on a
  subagent's say-so.
- Files are edited in place: work on the same files one after another; in parallel only on
  disjoint files or repositories.
- Gates must be green before review: run each repository's tests, lints and type checks
  (whatever its CLAUDE.md, AGENTS.md, Makefile, package manifest or CI configuration names) and
  fix what is red. Run a long suite as a twr job (`twr job run -- <command>`) and wait for its
  result: the loop goes on once you end your answer, not before. The reviewer reads your work
  next; it is not a substitute for the repository's own tests.
{{baseline}}
{{commit_rule}}

End your final message with one fenced block, info string `twr-implement`, and nothing after
it: whether the plan is implemented (`done`, or `partial`) and the gates you ran, like this
(without the indentation):

    ```twr-implement
    {"status": "done", "gates": {"ran": ["cargo test --workspace"], "result": "pass"}}
    ```

The review starts only from `done` and green gates. The hub reads only that block of your final
message.
