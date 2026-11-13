# One artifact, two runtimes: CC_ROLE=mgmt|control selects the feature set at boot.
# Build context is control-center/ (so app/ can be copied as a unit).
ARG MAVEN_IMAGE=docker.1ms.run/library/maven:3.9-eclipse-temurin-21
ARG JRE_IMAGE=docker.1ms.run/library/eclipse-temurin:21-jre

FROM ${MAVEN_IMAGE} AS build
WORKDIR /build
# Flaky-network tolerance: retry each artifact three times before failing the build.
ENV MAVEN_OPTS="-Dmaven.wagon.http.retryHandler.count=3 -Dmaven.wagon.rto=60000"
COPY app/ /build/app/
# Cache mount keeps the Maven repository between builds: changing a .java file no longer
# re-downloads the dependency tree.
RUN --mount=type=cache,target=/root/.m2 \
    mvn -B -ntp -f /build/app/pom.xml -DskipTests package

FROM ${JRE_IMAGE}

# The plan forbids a shell-less scratch image (we need curl for health probes and
# debugging), but a JRE is enough: no compiler, no build tools, no package manager use
# at runtime.
ARG APP_VERSION=dev
ENV TZ=UTC \
    LANG=C.UTF-8 \
    JAVA_OPTS="-XX:MaxRAMPercentage=75 -XX:+ExitOnOutOfMemoryError -Duser.timezone=UTC -Dfile.encoding=UTF-8" \
    CC_VERSION=${APP_VERSION}

RUN groupadd --system --gid 10001 cc \
 && useradd --system --uid 10001 --gid cc --home-dir /app --shell /usr/sbin/nologin cc

WORKDIR /app
COPY --from=build /build/app/target/cc-service-*.jar /app/app.jar
RUN chown -R 10001:10001 /app

USER 10001
EXPOSE 8080

# exec keeps the JVM as PID 1 so SIGTERM triggers Spring's graceful shutdown directly;
# no init shim is needed.
ENTRYPOINT ["sh", "-c", "exec java $JAVA_OPTS -jar /app/app.jar"]
