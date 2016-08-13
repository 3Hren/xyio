FROM ubuntu:trusty

# Download installer dependencies.
RUN apt-get update -y
RUN apt-get install -y curl gcc

# Install Rust.
RUN curl -s https://sh.rustup.rs > /home/install.sh
RUN chmod +x /home/install.sh
RUN sh /home/install.sh -y
RUN . $HOME/.cargo/env

WORKDIR /build/xyio
RUN $HOME/.cargo/bin/rustup override set nightly

# Cache dependencies.

# Copy and build.
COPY . /build/xyio/
RUN $HOME/.cargo/bin/cargo run --release -- --threads=1
