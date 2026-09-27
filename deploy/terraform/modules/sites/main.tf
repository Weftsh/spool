# The name customers' published sites answer to: a Route 53 hosted zone
# for `domain_name` and a certificate covering it and one level below.
#
# This is a *different registered domain* from the one in `modules/dns`,
# and that is a security decision rather than a cosmetic one. The
# dashboard's session cookie (`api/auth_api.rs`) is host-only and
# HttpOnly but carries no `__Host-` prefix, so a script on a page a
# customer published could set a same-named cookie on a parent domain
# that the dashboard cannot tell from its own. Sites on a separate
# registration — one filed with the Public Suffix List — cannot reach
# the product's cookies at all.
#
# Two things this module deliberately does not do:
#
#   * No per-site records. Every site is `<label>.<domain_name>` and they
#     all resolve through one wildcard to one CloudFront distribution;
#     the record and the distribution arrive with the serving slice.
#   * No waiter on the certificate. `modules/dns` blocks until ACM has
#     issued, which is right there because the whole stack is downstream
#     of it. Here nothing is downstream yet, and the delegation this zone
#     needs is a person at a registrar — so the certificate is *requested*
#     and its validation records written, ACM issues it by itself once
#     the registrar points at these name servers, and the slice that
#     first consumes the certificate is the slice that waits for it.

terraform {
  required_providers {
    aws = {
      source                = "hashicorp/aws"
      configuration_aliases = [aws.us_east_1]
    }
  }
}

variable "project" { type = string }
variable "env" { type = string }
variable "domain_name" {
  description = "The sites zone's apex: `weft.cx` in production. Customer sites live one label below it; the apex itself serves nothing yet."
  type        = string
}

resource "aws_route53_zone" "this" {
  name    = var.domain_name
  comment = "${var.project}-${var.env} — published sites"
}

# CloudFront reads certificates from us-east-1 and nowhere else, whatever
# region the rest of the stack is in — hence the aliased provider.
#
# The wildcard is the whole point: one certificate for every site we will
# ever publish under this name. It covers exactly one label, which is all
# a site gets (`<org>-<repo>.weft.cx`), so no site can be issued a name
# that a wildcard for a *deeper* level would have to cover.
resource "aws_acm_certificate" "this" {
  provider                  = aws.us_east_1
  domain_name               = var.domain_name
  subject_alternative_names = ["*.${var.domain_name}"]
  validation_method         = "DNS"

  lifecycle {
    create_before_destroy = true
  }
}

# The apex and the wildcard validate through the same CNAME; `for_each`
# over the set collapses the duplicate instead of fighting over it.
resource "aws_route53_record" "validation" {
  for_each = {
    for o in aws_acm_certificate.this.domain_validation_options :
    o.domain_name => {
      name   = o.resource_record_name
      type   = o.resource_record_type
      record = o.resource_record_value
    }
  }

  zone_id         = aws_route53_zone.this.zone_id
  name            = each.value.name
  type            = each.value.type
  ttl             = 60
  records         = [each.value.record]
  allow_overwrite = true
}

# ------------------------------------------------------------ no mail
#
# Nothing ever sends mail as this domain — the product's mail identity is
# in `modules/dns`, on the other name. A domain with no MX and no SPF is
# a domain anyone can forge a From: on, and this one will be sitting next
# to real customer content, so say so explicitly rather than by absence:
# RFC 7505's null MX refuses delivery outright, SPF authorises no sender
# at all, and DMARC asks receivers to reject what fails both.
resource "aws_route53_record" "null_mx" {
  zone_id = aws_route53_zone.this.zone_id
  name    = var.domain_name
  type    = "MX"
  ttl     = 3600
  records = ["0 ."]
}

resource "aws_route53_record" "spf" {
  zone_id = aws_route53_zone.this.zone_id
  name    = var.domain_name
  type    = "TXT"
  ttl     = 3600
  records = ["v=spf1 -all"]
}

resource "aws_route53_record" "dmarc" {
  zone_id = aws_route53_zone.this.zone_id
  name    = "_dmarc.${var.domain_name}"
  type    = "TXT"
  ttl     = 3600
  records = ["v=DMARC1; p=reject; adkim=s; aspf=s"]
}

output "zone_id" {
  value = aws_route53_zone.this.zone_id
}

output "name_servers" {
  description = "What the sites domain's registrar has to be told. Until it is, the certificate stays PENDING_VALIDATION and no site can be served."
  value       = aws_route53_zone.this.name_servers
}

output "certificate_arn" {
  description = "The requested certificate. It is not necessarily ISSUED — the slice that puts it on a distribution is the one that waits."
  value       = aws_acm_certificate.this.arn
}
