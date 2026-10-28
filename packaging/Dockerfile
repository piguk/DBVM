# The image for Kubernetes (packaging/k8s): a static (musl) binary on Alpine, with bubblewrap.
#
#   docker build -f packaging/Dockerfile -t selfdb .
FROM rust:alpine AS build
RUN apk add --no-cache musl-dev gcc
WORKDIR /src
COPY . .
RUN cargo build --release --locked

FROM alpine:latest
RUN apk add --no-cache bubblewrap ca-certificates
COPY --from=build /src/target/release/self /usr/local/bin/self
ENTRYPOINT ["/usr/local/bin/self"]
