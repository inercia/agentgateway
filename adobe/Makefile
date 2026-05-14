ADOBE_VERSION ?= $(shell cat $(dir $(lastword $(MAKEFILE_LIST)))/version)
PUBLIC_REMOTE ?= https://github.com/agentgateway/agentgateway.git
BUILD_FEATURES ?= ui,adobe
PLATFORM ?= linux/amd64

# Resolve upstream version from tags. Only consider upstream semver tags
# (`v*`) so that local Adobe release tags (`adobe-*`) do not get picked up
# by `git describe`. If git describe fails to find a tag, fetch tags from
# the public repo and retry exactly once.
UPSTREAM_VERSION ?= $(shell \
	v=$$(git describe --tags --match 'v*' --always --dirty 2>/dev/null); \
	if echo "$$v" | grep -q '^v'; then echo "$$v"; else \
		git fetch $(PUBLIC_REMOTE) --tags >/dev/null 2>&1; \
		git describe --tags --match 'v*' --always --dirty 2>/dev/null; \
	fi)
GIT_REVISION ?= $(shell git rev-parse HEAD)

SNAPSHOT_REGISTRY = docker-api-platform-snapshot.dr-uw2.adobeitc.com
RELEASE_REGISTRY = docker-api-platform-release.dr-uw2.adobeitc.com
IMAGE_REPO = apiplatform/v2
IMAGE_NAME = agentgateway

SNAPSHOT_TAG = $(UPSTREAM_VERSION)-adobe-$(ADOBE_VERSION)-amd64
RELEASE_TAG = $(UPSTREAM_VERSION)-adobe-$(ADOBE_VERSION)-amd64

SNAPSHOT_IMAGE = $(SNAPSHOT_REGISTRY)/$(IMAGE_REPO)/$(IMAGE_NAME):$(SNAPSHOT_TAG)
RELEASE_IMAGE = $(RELEASE_REGISTRY)/$(IMAGE_REPO)/$(IMAGE_NAME):$(RELEASE_TAG)

DOCKER_BUILDX = docker buildx build \
	--platform $(PLATFORM) \
	--build-arg BUILDER=musl \
	--build-arg VERSION=$(UPSTREAM_VERSION) \
	--build-arg GIT_REVISION=$(GIT_REVISION) \
	--build-arg CARGO_BUILD_FEATURES=$(BUILD_FEATURES) \
	--load \
	--progress=plain

.PHONY: check-version
check-version:
	@case "$(UPSTREAM_VERSION)" in \
		v*) ;; \
		*) echo "ERROR: UPSTREAM_VERSION '$(UPSTREAM_VERSION)' does not start with 'v'." >&2; \
		   echo "       Tags could not be resolved. Run 'make fetch-tags' or check network." >&2; \
		   exit 1 ;; \
	esac

.PHONY: snapshot release

snapshot: check-version
	$(DOCKER_BUILDX) -t $(SNAPSHOT_IMAGE) ..

release: check-version
	$(DOCKER_BUILDX) -t $(RELEASE_IMAGE) ..

.PHONY: fetch-tags
fetch-tags:
	@git fetch $(PUBLIC_REMOTE) --tags

.PHONY: print-snapshot-image print-release-image
print-snapshot-image: check-version
	@echo $(SNAPSHOT_IMAGE)

print-release-image: check-version
	@echo $(RELEASE_IMAGE)
