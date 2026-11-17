# A stand-in "other computer" for Agentloft over SSH

Linux in Docker with sshd, git, a `~/demo` project and `agentloftd` built
from this checkout. It is for trying "Connect to Another Computer…" without a
second Mac. Use the `fake` agent there: it costs no tokens. Real agents
aren't installed in the image.

```sh
# Build it (the first build takes a few minutes; later ones reuse the cache).
docker build -f packaging/ssh-test/Dockerfile -t agentloft-ssh-test .

# A key for this test only. Never use your own key here.
mkdir -p /private/tmp/loft-remote && cd /private/tmp/loft-remote
ssh-keygen -q -t ed25519 -N "" -f key
docker run -d --name agentloft-ssh-test -p 127.0.0.1:2222:22 \
  -v $PWD/key.pub:/authorized_keys:ro agentloft-ssh-test

# An ssh config of its own, and an `ssh` that uses it. The app runs
# `$AGENTLOFT_SSH` instead of /usr/bin/ssh when it is set.
cat > ssh_config <<CFG
Host loft-docker
  HostName 127.0.0.1
  Port 2222
  User loft
  IdentityFile $PWD/key
  IdentitiesOnly yes
  UserKnownHostsFile $PWD/known_hosts
  StrictHostKeyChecking accept-new
CFG
printf '#!/bin/sh\nexec /usr/bin/ssh -F %s/ssh_config "$@"\n' "$PWD" > ssh && chmod +x ssh
```

Open a window on it straight away, on a scratch home:

```sh
AGENTLOFT_HOME=/private/tmp/loft-remote/home AGENTLOFT_SSH=/private/tmp/loft-remote/ssh \
  AGENTLOFT_DEV_REMOTE=loft-docker target/debug/agentloft
```

Or open it without `AGENTLOFT_DEV_REMOTE`: File ▸ Connect to Another Computer…
(or ⌘K "Connect"), then type `loft-docker`.
