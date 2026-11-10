# FinGraph Server. Build from the repository root:
#   docker build -f server/Dockerfile -t fingraph-server .
FROM rust:1-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# only the server and the core it needs (the desktop's GTK and the phone bridge aren't built)
RUN sed -i 's|members = .*|members = ["crates/fingraph-core", "crates/fingraph-server"]|' Cargo.toml \
 && cargo build --release --locked -p fingraph-server \
 && strip target/release/fingraph-server

FROM ubuntu:26.04 AS runtime
# poppler and tesseract read statements exactly as on the desktop; curl fetches exchange rates
RUN apt-get update \
 && apt-get install -y --no-install-recommends poppler-utils tesseract-ocr tesseract-ocr-eng curl ca-certificates tzdata \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/fingraph-server /usr/local/lib/fingraph/fingraph-server
COPY server/fingraph-server.sh /usr/local/bin/fingraph-server
ENV FINGRAPH_DATA=/data FINGRAPH_STATEMENTS=/statements FINGRAPH_LISTEN=0.0.0.0:8743 PUID=1000 PGID=1000 TZ=Etc/UTC
VOLUME ["/data", "/statements"]
EXPOSE 8743
HEALTHCHECK --interval=1m --timeout=5s --start-period=20s CMD curl -fsk https://127.0.0.1:8743/api/v1/info >/dev/null || exit 1
LABEL org.opencontainers.image.title="FinGraph Server" \
      org.opencontainers.image.description="Holds the FinGraph ledger and your statement files for the desktop and phone apps" \
      org.opencontainers.image.source="https://github.com/Stolly345/fingraph" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
ENTRYPOINT ["/usr/local/bin/fingraph-server"]
