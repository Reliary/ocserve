# zen-probe: run the user's exact opencode ELF (1.18.31) in a disposable
# container for FreeTier-gate experiments. Host services stay read-only
# (AGENTS §2 rule 11 / TESTING §1.6). The ELF needs only GLIBC >= 2.17
# (verified with objdump) so bookworm runs it unmodified.
FROM debian:bookworm-slim
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && echo "wireshark-common wireshark-common/install-setuid boolean false" | debconf-set-selections \
 && apt-get install -y --no-install-recommends \
      nodejs curl ca-certificates tcpdump tshark \
 && rm -rf /var/lib/apt/lists/*
# /opt/oc/opencode, /mnt/cfg/* and /probe are provided as bind mounts by host.sh
WORKDIR /probe
ENTRYPOINT ["/bin/bash", "/probe/run.sh"]
