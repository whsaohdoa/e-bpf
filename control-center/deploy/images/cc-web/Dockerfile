ARG NODE_IMAGE=node:22-alpine
ARG NGINX_IMAGE=nginx:1.27-alpine

FROM ${NODE_IMAGE} AS build
WORKDIR /web
COPY web/package.json web/package-lock.json /web/
RUN npm ci --no-audit --no-fund
COPY web/ /web/
RUN npm run build

FROM ${NGINX_IMAGE}
COPY deploy/images/cc-web/nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=build /web/dist /usr/share/nginx/html
EXPOSE 80
