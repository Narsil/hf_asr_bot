# syntax=docker/dockerfile:experimental
FROM rust:1.58 as builder

ENV TZ=America/New_York
RUN apt-get update \
    && apt-get install -y ca-certificates tzdata openssl

ENV HOME=/home/root

WORKDIR $HOME/app

ADD src src
ADD Cargo.lock .
ADD Cargo.toml .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
	--mount=type=cache,target=/home/root/app/target \
	cargo install --path .


FROM debian:buster-slim

EXPOSE 80
ENV TZ=America/New_York
RUN apt-get update \
    && apt-get install -y ca-certificates tzdata openssl libopus-dev \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/cargo/bin/hf_asr_bot ./hf_asr_bot
CMD ["./hf_asr_bot"]
