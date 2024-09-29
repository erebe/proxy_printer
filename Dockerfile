ARG BUILDER_IMAGE=builder_cache

############################################################
# Cache image with all the deps
FROM rust:1.81-bookworm AS builder_cache

RUN echo 'deb https://deb.debian.org/debian unstable main' >> /etc/apt/sources.list  && \ 
  apt-get update && \  
  apt -t unstable install llvm-19 libpolly-19-dev -y
RUN rustup component add rustfmt clippy && cargo install --no-default-features bpf-linker

WORKDIR /build
COPY . ./


RUN cargo fmt --all -- --check --color=always || (echo "Use cargo fmt to format your code"; exit 1)
#RUN cargo clippy --all --all-features -- -D warnings || (echo "Solve your clippy warnings to succeed"; exit 1)

#RUN cargo test --all --all-features
#RUN just test "tcp://localhost:2375" || (echo "Test are failing"; exit 1)

#ENV RUSTFLAGS="-C link-arg=-Wl,--compress-debug-sections=zlib -C force-frame-pointers=yes"
#RUN cargo build --tests --all-features
#RUN cargo build --release --all-features


############################################################
# Builder for production image
FROM ${BUILDER_IMAGE} AS builder_release

WORKDIR /build
COPY . ./

ARG BIN_TARGET=--bins
ARG PROFILE=release

#ENV RUSTFLAGS="-C link-arg=-Wl,--compress-debug-sections=zlib -C force-frame-pointers=yes"
RUN cargo xtask build-ebpf --${PROFILE} 
RUN cargo build --profile=${PROFILE} ${BIN_TARGET}


############################################################
# Final image
FROM debian:bookworm-slim as final-image

RUN useradd -ms /bin/bash app && \
        apt-get update && \
        apt-get -y upgrade && \
        apt install -y --no-install-recommends dumb-init && \
        apt-get clean && \
        rm -rf /var/lib/apt/lists

WORKDIR /home/app

ARG PROFILE=release
COPY --from=builder_release  /build/target/${PROFILE}/turbine-lb turbine-lb

ENV RUST_LOG="INFO"
EXPOSE 8080

#USER app

ENTRYPOINT ["/usr/bin/dumb-init", "-v", "--"]
CMD ["/bin/sh", "-c", "exec /home/app/turbine-lb"]
