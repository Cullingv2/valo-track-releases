FROM node:22-alpine
WORKDIR /app
COPY server.mjs .
ENV NODE_ENV=production PORT=8787 DATA_DIR=/data
RUN mkdir -p /data && chown node:node /data
VOLUME /data
EXPOSE 8787
USER node
HEALTHCHECK --interval=60s --timeout=5s CMD wget -qO- http://127.0.0.1:8787/v1/health >/dev/null || exit 1
CMD ["node", "server.mjs"]
