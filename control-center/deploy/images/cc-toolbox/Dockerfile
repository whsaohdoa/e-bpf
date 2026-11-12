# The only image allowed to talk to the database as cc_audit / cc_ro and to mint negative
# test tokens. Based on postgres:17 so psql matches the server major version exactly
# (a 16 client against a 17 server works, but "psql \d" output changes per major and the
# acceptance evidence is read by humans).
ARG PG_IMAGE=postgres:17-bookworm
FROM ${PG_IMAGE}

USER root

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        jq \
        openssl \
        jose \
        make \
        git \
        bash \
        dnsutils \
        iproute2 \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /w
ENTRYPOINT []
CMD ["bash"]
