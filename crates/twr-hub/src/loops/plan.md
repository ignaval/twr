This thread now runs an implementation loop: you plan the task, implement it, and then twr runs
an adversarial reviewer on your work, round after round, while you are the implementer and the
arbiter of its findings. This is the loop's first step: the plan. Do not write code yet.

Never pause for the user: the loop asks them nothing. Decide everything from the codebase, the
conversation and sensible defaults. If the task is ambiguous about what to build, pick the most
reasonable interpretation, state it in the plan, and go on: state an interpretation instead of
asking.

THE TASK:
{{task}}

The repositories:
{{repos}}
{{baseline}}
Read the real code first. Then write the plan in your final message. It states:
- the goal and your interpretation (what "done" means);
- the affected repositories and files;
- ordered, concrete steps naming the functions, endpoints and migrations;
- data and migration changes (backfills, reversibility);
- edge cases, failure modes, security and data-integrity considerations;
- verification per part (the tests and gates that prove it);
- cross-repository contracts and their consumers.
Write it for a reviewer who can read the repositories but wasn't in this conversation. The hub
writes it to {{plan_path}}, and every later step of the loop works from that file.
{{workers}}
End your final message with one fenced block, info string `twr-plan`, and nothing after it:
whether the plan touches money paths, schemas or migrations, or cross-repository contracts
(`contract_risk`), and why in a line, like this (without the indentation):

    ```twr-plan
    {"contract_risk": false, "reason": "UI only: a new button on the reports page"}
    ```

A plan with contract risk gets one review by another model before you implement it. The hub
reads only that block of your final message; the message above it is the plan.
