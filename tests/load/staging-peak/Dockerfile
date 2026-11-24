FROM node:24.18.0-bookworm-slim
ARG MTC_HARNESS_SOURCE_SHA
LABEL org.opencontainers.image.revision=$MTC_HARNESS_SOURCE_SHA
ENV MTC_HARNESS_SOURCE_SHA=$MTC_HARNESS_SOURCE_SHA
WORKDIR /harness
COPY tests/load /harness/tests/load
USER node
EXPOSE 8080
ENTRYPOINT ["node", "tests/load/staging-peak/mock.ts"]
