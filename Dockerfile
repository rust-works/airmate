# airmate as a container: a static-enough, non-root, no-shell image.
#
#   docker build -t airmate .
#   docker run --rm -p 127.0.0.1:9997:9997 \
#     -e AIRMATE_UPSTREAM_URL=https://api.fireworks.ai/inference -e AIRMATE_API_KEY=... \
#     -v airmate-state:/state airmate
#
# Publish the port on loopback only (as above): the gateway inside the container listens on all
# interfaces, so the published address is what decides who can reach it.

FROM rust:1.88-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked && strip target/release/airmate

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/airmate /usr/local/bin/airmate
COPY packaging/docker/airmate.yaml /etc/airmate/airmate.yaml
# The state directory holds the gateway token, logs and ledger: mount a volume to keep them.
ENV XDG_STATE_HOME=/state \
    XDG_CONFIG_HOME=/etc \
    HOME=/state
VOLUME /state
EXPOSE 9997
ENTRYPOINT ["/usr/local/bin/airmate"]
CMD ["serve"]
