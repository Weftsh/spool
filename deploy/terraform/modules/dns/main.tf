# The name the product answers to: a Route 53 hosted zone for
# `domain_name` and a certificate covering it and one level below.
#
# This module exists only when a domain is configured, and it is the
# one part of the stack that cannot finish on its own: the certificate
# is DNS-validated, so nothing here is ISSUED until the registrar
# delegates `domain_name` to the zone's name servers. The first apply is
# therefore two steps — `-target` the zone, hand `name_servers` to the
# registrar, then apply the rest — and `docs/deployment-aws.md` says so.
#
# Records live in `modules/app`, next to the things they point at; this
# module only knows the zone, the certificate and the mail identity, so
# the app module can depend on them without either module depending on
# the other as a unit.

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
variable "aws_region" { type = string }
variable "domain_name" {
  description = "The zone's apex: `weft.sh` in production, or a delegated subdomain such as `test.weft.sh` for a rehearsal. The product is served at this name, `api.` and `www.` below it, and SSH at `ssh.` below it."
  type        = string
}

variable "parent_domain_name" {
  description = "The zone `domain_name` is delegated from, when that zone is ours too: `weft.sh` for a `test.weft.sh` rehearsal. The NS record goes into the parent's zone from here, so the rehearsal's first apply needs no registrar and its certificate validates in the same run. Empty when `domain_name` is the apex the registrar points at."
  type        = string
  default     = ""
}

resource "aws_route53_zone" "this" {
  name    = var.domain_name
  comment = "${var.project}-${var.env}"
}

# A rehearsal lives one label under production's name, and production's
# zone is in this account — so the delegation the registrar does for the
# apex, this does for the subdomain: the parent zone is looked up by
# name (the record is ours, the zone is the other environment's state's)
# and given our name servers.
data "aws_route53_zone" "parent" {
  count = var.parent_domain_name == "" ? 0 : 1
  name  = var.parent_domain_name
}

resource "aws_route53_record" "delegation" {
  count   = var.parent_domain_name == "" ? 0 : 1
  zone_id = data.aws_route53_zone.parent[0].zone_id
  name    = var.domain_name
  type    = "NS"
  ttl     = 300
  records = aws_route53_zone.this.name_servers
}

# CloudFront reads certificates from us-east-1 and nowhere else, whatever
# region the rest of the stack is in — hence the aliased provider.
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

# Blocks until ACM sees the records — which it cannot until the registrar
# points at this zone. A fresh apply with no delegation waits here, which
# is the right place to wait: nothing downstream can use an unissued cert.
resource "aws_acm_certificate_validation" "this" {
  # Under a parent of ours, the delegation is what makes the validation
  # record resolvable; without it this waits out its timeout.
  depends_on = [aws_route53_record.delegation]

  provider                = aws.us_east_1
  certificate_arn         = aws_acm_certificate.this.arn
  validation_record_fqdns = [for r in aws_route53_record.validation : r.fqdn]

  timeouts {
    create = "30m"
  }
}

# ---------------------------------------------------------------- mail

# Mail leaves from this name too. SES proves the domain by DKIM — three
# CNAMEs it hands back — and signs every message with it, which is what
# DMARC aligns on. The custom MAIL FROM keeps the envelope sender on a
# subdomain of ours instead of amazonses.com, so SPF aligns as well;
# `USE_DEFAULT_VALUE` falls back to SES's own envelope if the MX below
# has not propagated yet rather than refusing to send.
#
# What terraform cannot do: a new account sends only to verified
# addresses until SES production access is requested by a person, in the
# console. docs/deployment-aws.md says so.
resource "aws_sesv2_email_identity" "this" {
  email_identity = var.domain_name

  dkim_signing_attributes {
    next_signing_key_length = "RSA_2048_BIT"
  }
}

resource "aws_route53_record" "dkim" {
  count   = 3
  zone_id = aws_route53_zone.this.zone_id
  name    = "${aws_sesv2_email_identity.this.dkim_signing_attributes[0].tokens[count.index]}._domainkey.${var.domain_name}"
  type    = "CNAME"
  ttl     = 600
  records = ["${aws_sesv2_email_identity.this.dkim_signing_attributes[0].tokens[count.index]}.dkim.amazonses.com"]
}

resource "aws_sesv2_email_identity_mail_from_attributes" "this" {
  email_identity         = aws_sesv2_email_identity.this.email_identity
  behavior_on_mx_failure = "USE_DEFAULT_VALUE"
  mail_from_domain       = "mail.${var.domain_name}"
}

resource "aws_route53_record" "mail_from_mx" {
  zone_id = aws_route53_zone.this.zone_id
  name    = "mail.${var.domain_name}"
  type    = "MX"
  ttl     = 600
  records = ["10 feedback-smtp.${var.aws_region}.amazonses.com"]
}

resource "aws_route53_record" "mail_from_spf" {
  zone_id = aws_route53_zone.this.zone_id
  name    = "mail.${var.domain_name}"
  type    = "TXT"
  ttl     = 600
  records = ["v=spf1 include:amazonses.com -all"]
}

# Quarantine, not reject: everything we send is DKIM-signed, so a
# legitimate message never fails alignment — but a stray forwarder should
# land in spam, not vanish, while the domain is new.
resource "aws_route53_record" "dmarc" {
  zone_id = aws_route53_zone.this.zone_id
  name    = "_dmarc.${var.domain_name}"
  type    = "TXT"
  ttl     = 600
  records = ["v=DMARC1; p=quarantine; adkim=s"]
}

output "zone_id" {
  value = aws_route53_zone.this.zone_id
}

output "name_servers" {
  description = "What the registrar has to be told. Until it is, the certificate never issues and the apply waits."
  value       = aws_route53_zone.this.name_servers
}

output "certificate_arn" {
  description = "The ISSUED certificate — this is the validation's arn, not the certificate's, so consumers wait for issuance rather than for creation."
  value       = aws_acm_certificate_validation.this.certificate_arn
}

output "mail_identity_arn" {
  description = "The SES identity the app's credential may send as."
  value       = aws_sesv2_email_identity.this.arn
}
