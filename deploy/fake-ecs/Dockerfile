# The stand-in for the ECS control plane in the prod-parity stack. It is
# the ONE component of that stack that is not what we deploy: production
# talks to real ECS, and this answers the same two calls by starting the
# runner image with the local docker daemon instead. See fake-ecs.py for
# what it checks and what it deliberately does not pretend to be.
FROM python:3.12-alpine
RUN apk add --no-cache docker-cli
COPY fake-ecs.py /fake-ecs.py
ENTRYPOINT ["python3", "-u", "/fake-ecs.py"]
