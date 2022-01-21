FROM rust:1.58 as builder
ENV TZ=America/New_York
RUN apt-get update \
    && apt-get install -y ca-certificates tzdata openssl
COPY . .
RUN cargo build --release

FROM debian:buster-slim

EXPOSE 80
ENV TZ=America/New_York
RUN apt-get update \
    && apt-get install -y ca-certificates tzdata openssl libopus-dev \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder ./target/release/hf_asr_bot ./hf_asr_bot
CMD ["./hf_asr_bot"]
