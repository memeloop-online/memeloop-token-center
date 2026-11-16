FROM node:24.18.0-bookworm-slim
WORKDIR /harness
COPY tests/load /harness/tests/load
USER node
EXPOSE 8080
ENTRYPOINT ["node", "tests/load/staging-peak/mock.ts"]
