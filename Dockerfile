FROM debian:bookworm-slim AS build
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates build-essential pkg-config && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.80.0 --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
WORKDIR /src
COPY Cargo.toml ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl unzip ca-certificates fonts-liberation \
    libnss3 libnspr4 libdbus-1-3 libatk1.0-0 libatk-bridge2.0-0 libcups2 libdrm2 \
    libxcomposite1 libxdamage1 libxfixes3 libxrandr2 libgbm1 libpango-1.0-0 \
    libcairo2 libasound2 libxshmfence1 libx11-xcb1 libxkbcommon0 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
RUN set -eux; \
    URL=$(curl -fsSL https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json \
      | grep -o 'https://storage.googleapis.com/chrome-for-testing-public/[^"]*linux64/chrome-linux64.zip' | head -n1); \
    curl -fL -o /tmp/c.zip "$URL"; unzip -q /tmp/c.zip -d /app; mv /app/chrome-linux64 /app/chrome; rm /tmp/c.zip
COPY --from=build /src/target/release/cf-managed /app/cf-managed
ENV CHROME_BIN=/app/chrome/chrome
ENV HEADLESS=true
ENV BROWSERS=2
ENV TABS=10
ENV timeOut=90000
EXPOSE 408
CMD ["/app/cf-managed"]
