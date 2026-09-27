variable "project" {
  type    = string
  default = "stratum"
}

variable "env" {
  type    = string
  default = "prod"
}

variable "aws_region" {
  type    = string
  default = "us-east-1"
}

variable "vpc_cidr" {
  type    = string
  default = "10.40.0.0/16"
}

variable "image_tag" {
  description = "Image tag the service runs. The first apply uses the manually-pushed `bootstrap` tag; afterwards CD registers new revisions outside terraform (ignore_changes)."
  type        = string
  default     = "bootstrap"
}

variable "task_cpu" {
  type    = number
  default = 2048
}

variable "task_memory" {
  description = "MiB. Sized for 16 concurrent buffered 64 MiB pushes plus git subprocesses."
  type        = number
  default     = 8192
}

variable "desired_count" {
  type    = number
  default = 2
}

variable "max_count" {
  type    = number
  default = 10
}

variable "db_min_acu" {
  type    = number
  default = 0.5
}

variable "db_max_acu" {
  type    = number
  default = 4
}

variable "domain_name" {
  description = "Optional custom domain, e.g. `weft.sh`. Empty = v1 posture: CloudFront default cert on *.cloudfront.net + SSH on the NLB. Set, the stack owns the Route 53 zone, issues a DNS-validated certificate, serves at the apex plus `api.`/`www.`, puts SSH at `ssh.`, and STRATUM_PUBLIC_URL follows it. The first apply needs the registrar delegated to `name_servers` before the certificate can issue — see docs/deployment-aws.md."
  type        = string
  default     = ""
}

variable "sites_domain_name" {
  description = "Optional domain for customer-published static sites, e.g. `weft.cx`. Empty = the feature stays inert: no zone, no certificate, and the server's host dispatch is one string comparison that never matches. It must be a *different registration* from `domain_name`, not a subdomain of it — a page a customer publishes must not be able to set a cookie the dashboard receives. Like `domain_name`, its zone needs the registrar delegated to `sites_name_servers` before the certificate can issue."
  type        = string
  default     = ""

  validation {
    condition     = var.sites_domain_name == "" || var.domain_name == "" || !endswith(var.sites_domain_name, ".${var.domain_name}")
    error_message = "sites_domain_name must not sit under domain_name: customer pages there could set cookies the dashboard cannot distinguish from its own. Register a separate domain."
  }
}

variable "parent_domain_name" {
  description = "For an environment served under another environment's domain — `weft.sh` when `domain_name` is `test.weft.sh`. The dns module then delegates `domain_name` itself, with an NS record in the parent's zone, so the first apply is one step instead of a trip to the registrar. Empty for the environment that owns the apex."
  type        = string
  default     = ""

  validation {
    condition     = var.parent_domain_name == "" || (var.domain_name != "" && endswith(var.domain_name, ".${var.parent_domain_name}"))
    error_message = "parent_domain_name must be the zone domain_name sits directly under (domain_name = test.weft.sh, parent_domain_name = weft.sh)."
  }
}

variable "origin_shield_region" {
  description = "Region for CloudFront Origin Shield. Empty = the deploy region (aws_region). Override only if aws_region is one of the few regions without Origin Shield support — point it at the nearest supported region."
  type        = string
  default     = ""
}

variable "cdn_url_ttl_secs" {
  description = "Lifetime of a signed CDN pack URL. Must comfortably exceed a slow clone: git does not fall back to the server for an advertised pack, so a URL that expires mid-clone fails it outright."
  type        = number
  default     = 3600
}

variable "ci_log_retention_days" {
  description = "Days a CI job log survives under ci/logs/ in the store bucket before S3 expires it."
  type        = number
  default     = 90
}

variable "cache_retention_days" {
  description = "Days a build-cache archive saved by a job on a Weft runner is kept, from its save (an S3 lifecycle rule on cache/ in the store bucket). Mirrors STRATUM_CACHE_RETENTION_DAYS on the app; change both together."
  type        = number
  default     = 10
}

# ------------------------------------------------------------ hosted runners

variable "control_plane_domain" {
  description = "Host that runners call back on, and the one non-build domain the runner firewall allows. Empty (the default) uses the CloudFront domain, which is what STRATUM_PUBLIC_URL points at. Set it only if you front the control plane with something else — an entry here does not replace the CloudFront domain in the allowlist, it replaces the callback host entirely."
  type        = string
  default     = ""
}

variable "runner_vpc_cidr" {
  description = "CIDR for the runner VPC. Must not overlap vpc_cidr — not because anything routes between them (nothing does), but so a future operator reading a flow log can tell the two apart."
  type        = string
  default     = "10.41.0.0/16"
}

variable "runner_image_tag" {
  description = "Tag of the weft-runner image. Same bootstrap dance as image_tag: the first apply needs `bootstrap` already pushed to ECR."
  type        = string
  default     = "bootstrap"
}

variable "github_runner_image_tag" {
  description = "Tag of the GitHub Actions runner image (Dockerfile.github-runner) in the `-github` ECR repository. Same bootstrap dance as runner_image_tag: the first apply needs `bootstrap` already pushed."
  type        = string
  default     = "bootstrap"
}

variable "runner_task_cpu" {
  type    = number
  default = 1024
}

variable "runner_task_memory" {
  description = "MiB. One job per task; sized for a compiler, not for a fleet."
  type        = number
  default     = 2048
}

variable "runner_max_procs" {
  description = "Processes one hosted job may create (RLIMIT_NPROC, enforced by the runner itself — Fargate accepts no nproc ulimit). null uses the runner binary's own default of 4096."
  type        = number
  default     = null
}

variable "runner_minutes_per_month" {
  description = "Hosted-runner minutes one organisation may burn per rolling 30 days when it has no override of its own. `0` is unmetered. `orgs.ci_minutes_per_month` overrides this per organisation — see docs/deployment-aws.md."
  type        = number
  default     = 2000
}

variable "runner_max_timeout_minutes" {
  description = "The largest `timeout-minutes` this fleet accepts. A workflow asking for more is refused at trigger time rather than clamped."
  type        = number
  default     = 360
}

variable "runner_log_retention_days" {
  description = "Retention for the runner's own container stderr and the firewall alert log. Build output does not live here — it goes to the store bucket under ci/logs/ (see ci_log_retention_days)."
  type        = number
  default     = 7
}

variable "runner_egress_allow_domains" {
  description = "Everywhere a build may reach. A leading '.' matches the domain and all subdomains; anything not listed is dropped by AWS Network Firewall, which is what stops a build dialling a mining pool. Extend it deliberately — every entry is somewhere tenant code can send bytes."
  type        = list(string)
  default = [
    # Source and package registries a build realistically needs. A
    # leading dot already covers the apex — Network Firewall reads
    # ".github.com" as github.com *and* its subdomains — and it refuses
    # a list that names the apex twice ("Targets has duplicate values"),
    # which is how a bare "github.com" beside this entry failed the first
    # real apply. Do not add one.
    ".github.com",
    "objects.githubusercontent.com",
    "pkg-containers.githubusercontent.com",
    # GitHub Actions runners (Dockerfile.github-runner) live in the same
    # VPC behind the same firewall. The official agent needs these to
    # register, take a job, fetch actions and report: api.github.com is
    # already inside ".github.com"; the rest are the job queue and the
    # results, log and cache services, the Azure blob storage those are
    # served from, and where `actions/checkout` and release assets are
    # downloaded. Listed by name rather than as ".githubusercontent.com"
    # so that raw.githubusercontent.com — a fetch-anything-anyone-hosts
    # endpoint — stays out.
    ".actions.githubusercontent.com",
    "codeload.github.com",
    "results-receiver.actions.githubusercontent.com",
    ".blob.core.windows.net",
    "objects-origin.githubusercontent.com",
    "github-releases.githubusercontent.com",
    "github-registry-files.githubusercontent.com",
    ".pkg.github.com",
    "github-cloud.s3.amazonaws.com",
    "release-assets.githubusercontent.com",
    "ghcr.io",
    "registry.npmjs.org",
    "crates.io",
    "static.crates.io",
    "index.crates.io",
    "pypi.org",
    "files.pythonhosted.org",
    "deb.debian.org",
    "security.debian.org",
    # The GitHub Actions runner image builds on Ubuntu 24.04
    # (mcr.microsoft.com/dotnet/runtime-deps:8.0-noble) and apt fetches
    # its indexes from these over plain HTTP; the fleet build of that
    # image got as far as the base image and every `Get:` timed out.
    "archive.ubuntu.com",
    "security.ubuntu.com",
    # Where a job fetches its own toolchain. The GitHub Actions runner
    # image (Dockerfile.github-runner) carries no Rust, Node or MinIO
    # and its jobs are not root, so `dtolnay/rust-toolchain`,
    # `actions/setup-node`, `playwright install` and a `curl` of a
    # pinned MinIO do the work on each job — from here. rustup is two
    # hosts: sh.rustup.rs serves the install script, which then fetches
    # rustup-init and every toolchain from static.rust-lang.org — the
    # first run on the fleet listed only the second and died in
    # `curl https://sh.rustup.rs` with SSL_ERROR_SYSCALL, which is what a
    # dropped connection looks like from inside. setup-node prefers
    # GitHub's own release mirror (already allowed) and falls back to
    # nodejs.org; Playwright downloads its browser from either of its
    # two CDN names. These are vendors' download hosts, not upload
    # endpoints, but they are still somewhere a build can reach.
    "sh.rustup.rs",
    "static.rust-lang.org",
    "nodejs.org",
    "dl.min.io",
    "cdn.playwright.dev",
    "playwright.download.prss.microsoft.com",
    # Where cdn.playwright.dev actually sends the browser: a 307 to the
    # Chrome for Testing bucket. The first web job on the fleet found it
    # (five "socket disconnected" retries, then "Failed to install
    # browsers"). SNI cannot scope a bucket host to one path, so this is
    # every public object on storage.googleapis.com; the same trade as
    # objects.githubusercontent.com above.
    "storage.googleapis.com",
    # The other toolchains a GitHub Actions job installs for itself, by
    # ecosystem. Each is a vendor's download host or package index, the
    # same class as crates.io and npm above: a build can fetch from it
    # and cannot use it to send data anywhere. Cloud APIs and coverage
    # uploaders are deliberately absent — those are places a build can
    # *put* bytes, and belong to a per-customer decision, not the default.
    # Python: uv's installer (pip and setup-python are covered above).
    # astral.sh/uv/install.sh is a redirect to releases.astral.sh, which
    # serves the installer and the uv binary itself; with only the first
    # listed, `curl … astral.sh/uv/install.sh | sh` died in SSL_connect on
    # the live fleet — the registry's runner leg found it on 2026-09-24.
    "astral.sh",
    "releases.astral.sh",
    # Go: modules and the checksum database; setup-go fetches the
    # toolchain from GitHub's release mirror first.
    "proxy.golang.org",
    "sum.golang.org",
    "go.dev",
    # Java: Temurin JDKs, Maven Central, Gradle.
    "api.adoptium.net",
    "repo.maven.apache.org",
    "repo1.maven.org",
    "services.gradle.org",
    "plugins.gradle.org",
    # Ruby gems; setup-ruby fetches Ruby itself from GitHub releases.
    "rubygems.org",
    "index.rubygems.org",
    # .NET SDKs and NuGet.
    "builds.dotnet.microsoft.com",
    "api.nuget.org",
    # JavaScript beyond npm: Yarn classic, Bun, Deno and JSR.
    "registry.yarnpkg.com",
    "bun.sh",
    "deno.land",
    "jsr.io",
    # PHP: Composer and Packagist.
    "getcomposer.org",
    "repo.packagist.org",
    # Terraform binaries and providers.
    "releases.hashicorp.com",
    "registry.terraform.io",
    # Microsoft Container Registry, for Dockerfile.github-runner's base
    # (the .NET runtime dependencies actions/runner needs). Found the
    # same way as index.docker.io: the first fleet build of that image
    # timed out on the TLS handshake to it.
    "mcr.microsoft.com",
    # …and the regional hosts it redirects layer downloads to
    # (eastus.data.mcr.microsoft.com from the fleet; the region follows
    # the client, so the wildcard). The manifest fetch passed with the
    # line above alone and the first blob timed out, on the next run.
    ".data.mcr.microsoft.com",
    # Docker Hub, for builds that pull a base image. index.docker.io is
    # the front door every daemon-less client dials first (kaniko, crane,
    # skopeo, go-containerregistry); the daemon goes straight to
    # registry-1. Without it a kaniko build on the fleet timed out on
    # the TLS handshake before its first manifest (deploy/proot,
    # 2026-09-22).
    "index.docker.io",
    "registry-1.docker.io",
    "auth.docker.io",
    "production.cloudflare.docker.com",
  ]
}

variable "billing_enabled" {
  description = "Sell seats through Stripe. Off, every organisation holds private repositories for free (the self-hosted build). On, the `<project>/<env>/stripe` secret must be filled — see modules/data — and the plan refuses if any of its three values is blank."
  type        = bool
  default     = false
}

variable "github_app_slug" {
  description = "Slug of the GitHub App this fleet is (`weft` for https://github.com/apps/weft) — what `scripts/github-app-create.py` printed. Empty = no GitHub provider: no mirrors or imports, and the dashboard's Connect button answers 501. Set, the `<project>/<env>/github-app` secret must be filled — see modules/data — and the plan refuses if any of its three values is blank."
  type        = string
  default     = ""
}

variable "github_runners_app_slug" {
  description = "Slug of a second GitHub App that only registers runners and cancels runs (`weftsh-runners` for https://github.com/apps/weftsh-runners). A Marketplace listing is per App, and the two products want different permissions: mirroring reads contents, runners write administration. Empty = the one App above does both. Set, the `<project>/<env>/github-runners-app` secret must be filled the same way as the mirror App's, and `github_app_slug` must be set too."
  type        = string
  default     = ""
}

variable "mail_from" {
  description = "Address the fleet sends mail as, through SES. Only meaningful with `domain_name`, and must be under it — the domain is the one identity SES is told to trust. Empty = `no-reply@<domain_name>` when there is a domain, and no mail at all when there is not."
  type        = string
  default     = ""

  validation {
    condition     = var.mail_from == "" || (var.domain_name != "" && endswith(var.mail_from, "@${var.domain_name}"))
    error_message = "mail_from needs domain_name and must be an address under it: the domain is the one identity SES is told to sign for."
  }
}

variable "free_ci_minutes" {
  description = "Hosted-runner minutes per rolling 30 days for a person and for an organisation with no paid seats (public repositories only). `0` is unmetered."
  type        = number
  default     = 500
}

variable "paid_ci_minutes_per_seat" {
  description = "Hosted-runner minutes per billing period that each paid seat adds to its organisation's pool. `0` is unmetered."
  type        = number
  default     = 1000
}

variable "paid_egress_gb_per_seat" {
  description = "Gigabytes served out of private repositories per billing period that each paid seat adds to the pool. `0` is unmetered."
  type        = number
  default     = 10
}

variable "paid_storage_gb_per_seat" {
  description = "Gigabytes stored in private repositories, averaged over the billing period, that each paid seat adds to the pool. `0` is unmetered."
  type        = number
  default     = 5
}

variable "paid_packages_gb_per_seat" {
  description = "Gigabytes of package-registry storage, averaged over the billing period, that each paid seat adds to the pool. Its own pool because it is its own meter at its own price. `0` is unmetered."
  type        = number
  default     = 2
}

variable "price_per_seat_cents" {
  description = "What the dashboard and the pricing page print for one seat per month, in cents. Display only: the Stripe price object named in the secret is what is charged, and the two must agree."
  type        = number
  default     = 400
}

variable "overage_1000_minutes_cents" {
  description = "What the dashboard quotes for a thousand hosted minutes past the pool, in cents ($0.008 a minute is 800). Display only: the metered price in the secret charges."
  type        = number
  default     = 800
}

variable "overage_egress_gb_cents" {
  description = "What the dashboard quotes for a gigabyte served past the pool, in cents. Display only."
  type        = number
  default     = 10
}

variable "overage_storage_gb_month_cents" {
  description = "What the dashboard quotes for a gigabyte-month stored past the pool, in cents. Display only."
  type        = number
  default     = 10
}

variable "overage_packages_gb_month_cents" {
  description = "What the dashboard quotes for a gigabyte-month of packages past the pool, in cents. Display only."
  type        = number
  default     = 15
}

variable "billing_rollup_secs" {
  description = "How often the server folds usage and reports overage to Stripe's meters. Fifteen minutes, so the last hour of a billing period lands inside the meters' grace window."
  type        = number
  default     = 900
}

variable "storage_sweep_secs" {
  description = "How often every repository's stored bytes are re-read from its manifest and the day's storage sample is taken. `0` disables the sweep (and storage metering with it)."
  type        = number
  default     = 3600
}

variable "storage_inventory_secs" {
  description = "How often the bucket is listed to record physical bytes next to the manifest's logical bytes. Never billed; it is how a leaking cache or a broken GC is noticed."
  type        = number
  default     = 86400
}

variable "stripe_meter_minutes" {
  description = "The `event_name` of the Billing Meter hosted minutes past the pool are reported under. Public, like every event name; the matching metered price id lives in the secret as `price_minutes`."
  type        = string
  default     = "weft_hosted_minutes"
}

variable "stripe_meter_egress" {
  description = "The `event_name` of the Billing Meter megabytes served past the pool are reported under. Its price is `price_egress` in the secret."
  type        = string
  default     = "weft_private_egress_mb"
}

variable "stripe_meter_storage" {
  description = "The `event_name` of the Billing Meter megabyte-days stored past the pool are reported under. Its price is `price_storage` in the secret."
  type        = string
  default     = "weft_private_storage_mb_days"
}

variable "stripe_meter_packages" {
  description = "The `event_name` of the Billing Meter megabyte-days of package storage past the pool are reported under. Its price is `price_packages` in the secret."
  type        = string
  default     = "weft_private_packages_mb_days"
}

variable "stripe_price_packages" {
  description = "The metered price for package storage past the pool, when the Stripe secret has no `price_packages` key — a secret filled before the fourth meter existed. A price id is not secret; the secret's value wins when it has one."
  type        = string
  default     = ""
}
