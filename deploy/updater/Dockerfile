FROM docker:29.3.1-cli@sha256:18f5ab0fab739ea822819b342357947dfba235cdef438cce345ebc0c143c5b34
RUN apk add --no-cache python3 openssl
COPY update.py /usr/local/bin/rusttalk-updater
ENTRYPOINT ["python3", "-u", "/usr/local/bin/rusttalk-updater"]
