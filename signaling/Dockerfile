# Signaling server image: the Go relay plus the viewer page it serves.
# Build from the repository root, so client/ is in the build context:
#   docker build -f signaling/Dockerfile -t remotebridge-signaling .

# Build stage: the Go version is pinned here, so the server is always built
# with a known toolchain (not whatever the host machine has installed).
FROM golang:1.27.2-alpine AS build
WORKDIR /src
COPY signaling/go.mod signaling/go.sum ./
RUN go mod download
COPY signaling/*.go ./
# CGO off gives a static binary that runs on an image with no C library.
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /remotebridge-signaling .

# Run stage: distroless has no shell or package manager, only the binary,
# and the nonroot tag runs it as an unprivileged user.
FROM gcr.io/distroless/static-debian12:nonroot
# The server serves ../client relative to its working directory, so the
# layout matches the repository: /app/signaling next to /app/client.
WORKDIR /app/signaling
COPY --from=build /remotebridge-signaling ./
COPY client/ /app/client/
# Inside a container the server must listen on all interfaces; the host
# side decides what is exposed (-p 127.0.0.1:8080:8080 behind Caddy).
ENV REMOTEBRIDGE_ADDR=:8080
EXPOSE 8080
ENTRYPOINT ["/app/signaling/remotebridge-signaling"]
