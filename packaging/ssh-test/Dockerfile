# A stand-in "other machine" for trying Agentloft over SSH: Linux with sshd,
# git and agentloftd built from this checkout. See README.md here.
#
#   docker build -f packaging/ssh-test/Dockerfile -t agentloft-ssh-test .
FROM rust:1.99-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target-linux \
    CARGO_TARGET_DIR=/src/target-linux cargo build --release -p loftd \
    && cp /src/target-linux/release/agentloftd /agentloftd

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends openssh-server git ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /run/sshd \
    && useradd -m -s /bin/bash loft \
    && sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config
COPY --from=build /agentloftd /usr/local/bin/agentloftd
# A project to open, with some history.
RUN su loft -c 'mkdir -p ~/demo && cd ~/demo && git init -q -b main \
    && printf "# Demo\n\nA project on the other machine.\n" > README.md \
    && git add . && git -c user.name=loft -c user.email=loft@example.com commit -qm "First commit"'
# The client's public key is mounted at /authorized_keys.
CMD install -o loft -g loft -m 700 -d /home/loft/.ssh \
    && install -o loft -g loft -m 600 /authorized_keys /home/loft/.ssh/authorized_keys \
    && exec /usr/sbin/sshd -D -e
