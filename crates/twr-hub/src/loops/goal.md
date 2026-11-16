This thread now runs a review loop on its own work: twr runs an adversarial reviewer on it,
round after round, and you are the implementer and the arbiter. This is the loop's first step.

1. Run this repository's quality gates once (its tests, lints and type checks: whatever its
   CLAUDE.md, AGENTS.md, Makefile, package manifest or CI configuration names), so that the
   loop starts from green. Fix what is red first. Run a long suite as a twr job
   (`twr job run -- <command>`) and wait for its result: the loop goes on once you end your
   answer, not before.
2. Write one paragraph, the GOAL: what the changes under review are for, what they must do,
   and what is deliberately out of scope. The reviewer reads it every round.

The review covers {{scope}}:
{{repos}}

End your final message with the GOAL paragraph in a fenced block of its own, info string
`twr-goal`, and nothing after it, like this (without the indentation):

    ```twr-goal
    <one paragraph>
    ```

The hub reads only that block of your final message. The loop's files are in {{folder}}.
