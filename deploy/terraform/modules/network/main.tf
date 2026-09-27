# Two-AZ VPC: public subnets carry the load balancers, private subnets
# carry the tasks (no public IPs). One NAT gateway — a deliberate
# cost/availability trade documented in docs/deployment-aws.md; the S3
# gateway endpoint keeps the heavy traffic (clone range-GETs against the
# store bucket) off the NAT entirely, so NAT egress is only mirror-origin
# fetches, tenant webhooks, and image pulls.

variable "project" { type = string }
variable "env" { type = string }
variable "vpc_cidr" { type = string }

data "aws_availability_zones" "available" {
  state = "available"
}

locals {
  azs = slice(data.aws_availability_zones.available.names, 0, 2)
}

resource "aws_vpc" "this" {
  cidr_block           = var.vpc_cidr
  enable_dns_support   = true
  enable_dns_hostnames = true

  tags = { Name = "${var.project}-${var.env}" }
}

resource "aws_internet_gateway" "this" {
  vpc_id = aws_vpc.this.id
}

resource "aws_subnet" "public" {
  count                   = 2
  vpc_id                  = aws_vpc.this.id
  cidr_block              = cidrsubnet(var.vpc_cidr, 4, count.index)
  availability_zone       = local.azs[count.index]
  map_public_ip_on_launch = true

  tags = { Name = "${var.project}-${var.env}-public-${count.index}" }
}

resource "aws_subnet" "private" {
  count             = 2
  vpc_id            = aws_vpc.this.id
  cidr_block        = cidrsubnet(var.vpc_cidr, 4, count.index + 8)
  availability_zone = local.azs[count.index]

  tags = { Name = "${var.project}-${var.env}-private-${count.index}" }
}

resource "aws_eip" "nat" {
  domain = "vpc"
}

resource "aws_nat_gateway" "this" {
  allocation_id = aws_eip.nat.id
  subnet_id     = aws_subnet.public[0].id
  depends_on    = [aws_internet_gateway.this]
}

resource "aws_route_table" "public" {
  vpc_id = aws_vpc.this.id

  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.this.id
  }
}

resource "aws_route_table_association" "public" {
  count          = 2
  subnet_id      = aws_subnet.public[count.index].id
  route_table_id = aws_route_table.public.id
}

resource "aws_route_table" "private" {
  vpc_id = aws_vpc.this.id

  route {
    cidr_block     = "0.0.0.0/0"
    nat_gateway_id = aws_nat_gateway.this.id
  }
}

resource "aws_route_table_association" "private" {
  count          = 2
  subnet_id      = aws_subnet.private[count.index].id
  route_table_id = aws_route_table.private.id
}

# Clone traffic is range-GET volume against S3; the gateway endpoint
# routes it inside AWS for free instead of through the NAT.
data "aws_region" "current" {}

resource "aws_vpc_endpoint" "s3" {
  vpc_id            = aws_vpc.this.id
  service_name      = "com.amazonaws.${data.aws_region.current.name}.s3"
  vpc_endpoint_type = "Gateway"
  route_table_ids   = [aws_route_table.private.id]
}

output "vpc_id" {
  value = aws_vpc.this.id
}

output "public_subnet_ids" {
  value = aws_subnet.public[*].id
  # A load balancer in a public subnet holds public addresses, and EC2
  # refuses to detach the internet gateway while any are mapped. Nothing
  # in the resource graph said so, so `destroy` reached for the gateway
  # with the ALB still up and spent twenty minutes retrying a
  # DependencyViolation. Carrying the gateway on this output orders
  # every consumer of a public subnet after it, in both directions.
  depends_on = [aws_internet_gateway.this]
}

output "private_subnet_ids" {
  value = aws_subnet.private[*].id
}
