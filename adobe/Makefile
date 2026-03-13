ADOBE_VERSION ?= $(shell cat $(dir $(lastword $(MAKEFILE_LIST)))/version)
ARCH ?= $(shell uname -m | sed 's/x86_64/amd64/' | sed 's/aarch64/arm64/')
PUBLIC_REMOTE ?= https://github.com/agentgateway/agentgateway.git

# Resolve upstream version from tags. If git describe fails to find a tag,
# fetch tags from the public repo and retry exactly once.
UPSTREAM_VERSION ?= $(shell \
	v=$$(git describe --tags --always --dirty 2>/dev/null); \
	if echo "$$v" | grep -q '^v'; then echo "$$v"; else \
		git fetch $(PUBLIC_REMOTE) --tags >/dev/null 2>&1; \
		git describe --tags --always --dirty 2>/dev/null; \
	fi)

SNAPSHOT_REGISTRY = docker-api-platform-snapshot.dr-uw2.adobeitc.com
RELEASE_REGISTRY = docker-api-platform-release.dr-uw2.adobeitc.com
IMAGE_REPO = apiplatform/v2

SNAPSHOT_TAG = $(UPSTREAM_VERSION)-adobe-$(ADOBE_VERSION)-$(ARCH)
RELEASE_TAG = $(UPSTREAM_VERSION)-adobe-$(ADOBE_VERSION)

IMAGE_NAME = agentgateway
SNAPSHOT_IMAGE = $(SNAPSHOT_REGISTRY)/$(IMAGE_REPO)/$(IMAGE_NAME):$(SNAPSHOT_TAG)
RELEASE_IMAGE = $(RELEASE_REGISTRY)/$(IMAGE_REPO)/$(IMAGE_NAME):$(RELEASE_TAG)

.PHONY: check-version
check-version:
	@case "$(UPSTREAM_VERSION)" in \
		v*) ;; \
		*) echo "ERROR: UPSTREAM_VERSION '$(UPSTREAM_VERSION)' does not start with 'v'." >&2; \
		   echo "       Tags could not be resolved. Run 'make fetch-tags' or check network." >&2; \
		   exit 1 ;; \
	esac

.PHONY: snapshot release snapshot-ci release-ci

snapshot: check-version
	$(MAKE) -C .. docker DOCKER_REGISTRY=$(SNAPSHOT_REGISTRY) DOCKER_REPO=$(IMAGE_REPO) IMAGE_TAG=$(SNAPSHOT_TAG)

snapshot-ci: check-version
	$(MAKE) -C .. docker-ci DOCKER_REGISTRY=$(SNAPSHOT_REGISTRY) DOCKER_REPO=$(IMAGE_REPO) IMAGE_TAG=$(SNAPSHOT_TAG)

release: check-version
	$(MAKE) -C .. docker DOCKER_REGISTRY=$(RELEASE_REGISTRY) DOCKER_REPO=$(IMAGE_REPO) IMAGE_TAG=$(RELEASE_TAG)

release-ci: check-version
	$(MAKE) -C .. docker-ci DOCKER_REGISTRY=$(RELEASE_REGISTRY) DOCKER_REPO=$(IMAGE_REPO) IMAGE_TAG=$(RELEASE_TAG)

.PHONY: fetch-tags
fetch-tags:
	@git fetch $(PUBLIC_REMOTE) --tags

.PHONY: print-snapshot-image print-release-image
print-snapshot-image: check-version
	@echo $(SNAPSHOT_IMAGE)

print-release-image: check-version
	@echo $(RELEASE_IMAGE)
