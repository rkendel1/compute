FROM debian:bookworm-slim

ARG COMPUTE_DISTRIBUTION=compute-distribution
COPY ${COMPUTE_DISTRIBUTION}/ /opt/compute/

ENV COMPUTE_DISTRIBUTION_ROOT=/opt/compute
ENV PATH=/opt/compute/bin

ENTRYPOINT ["/opt/compute/bin/compute"]
CMD ["runtimes"]
