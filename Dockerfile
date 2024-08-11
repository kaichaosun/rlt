FROM alpine:3.20 AS downloader

# Define build argument for version
ARG RLT_VERSION

WORKDIR /app

# Install necessary tools
RUN apk add --no-cache curl tar

RUN curl -L https://github.com/zeroows/rlt/releases/download/${RLT_VERSION}/rlt-${RLT_VERSION}-x86_64-unknown-linux-musl.tar.gz | tar xz

FROM alpine:3.20

WORKDIR /app

COPY --from=downloader /app/localtunnel /app

ENV DOMAIN=init.so
ENV PORT=3000
ENV PROXY_PORT=3001

# Expose the ports
EXPOSE ${PORT}
EXPOSE ${PROXY_PORT}

# Run with strace to capture system calls
ENTRYPOINT ["strace", "-f", "-e", "trace=all", "-s", "1024", "/app/localtunnel"]
CMD ["server", "--domain", "${DOMAIN}", "--port", "${PORT}", "--proxy-port", "${PROXY_PORT}", "--secure"]