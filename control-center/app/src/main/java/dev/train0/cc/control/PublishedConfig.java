package dev.train0.cc.control;

/**
 * Result of publishing one {@code ConfigDocument}: the immutable version number, the sha256 of
 * exactly the bytes that were persisted, and their length.
 *
 * <p>The bytes are not returned: agents fetch them through {@code GetConfig}, and the HTTP publish
 * endpoint only needs the version and hash to report success.
 */
record PublishedConfig(int version, String contentSha256, int bytes) {}
