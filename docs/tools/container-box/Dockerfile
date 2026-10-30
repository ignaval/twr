# A container for a twr box: the hub reaches it through `docker exec -i`, and the threads you
# start there run inside it (docs/runners.md#container-boxes). Build it with
#
#   docker build -t twr-box docs/tools/container-box
#
# The image has what twr needs on a box (sh, the coreutils, git) and a plain user, box, whose
# home the recipe keeps in a named volume. twr itself is not installed here: the hub uploads
# its own build on the first connection.
#
# Agents: install them in a derived image, or in the running container as box, and log them in
# there (`twr box login`, or the agent's own login through `docker exec -it`). For example,
# with Node in the image: `npm install -g @anthropic-ai/claude-code @openai/codex`, or Claude
# Code's native installer (`curl -fsSL https://claude.ai/install.sh | bash`, as box).
#
# A development build of twr is linked against the glibc of the machine that built it: BASE
# needs that version or newer (ubuntu:24.04 has 2.39). A release build runs on any Linux.
ARG BASE=ubuntu:24.04
FROM ${BASE}

RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates procps \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -s /bin/bash box
USER box
WORKDIR /home/box

# PID 1 only waits: run the container with --init so that processes left behind are reaped.
CMD ["sleep", "infinity"]
