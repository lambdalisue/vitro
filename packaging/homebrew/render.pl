#!/usr/bin/env perl
# Fill in the formula template from a release's version and checksum files.
#
#   perl render.pl <version> <directory of vitro-*.sha256> < vitro.rb > Formula/vitro.rb
#
# Fails rather than writing a formula with a placeholder left in it: a
# formula naming no checksum installs nothing, and one naming the wrong
# archive installs the wrong thing.
use strict;
use warnings;

my ($version, $dist) = @ARGV;
die "usage: render.pl <version> <dist dir> < template\n" unless defined $dist;

my %sha256;
for my $file (glob "$dist/vitro-v$version-*.sha256") {
    open my $fh, '<', $file or die "cannot read $file: $!\n";
    my ($sum, $name) = split ' ', scalar <$fh>;
    my ($target) = $name =~ /^vitro-v\Q$version\E-(.+)\.(?:tar\.gz|zip)$/
        or die "unexpected archive name in $file: $name\n";
    $sha256{$target} = $sum;
}

local $/;
my $formula = <STDIN>;
$formula =~ s/\@VERSION\@/$version/g;
$formula =~ s{\@SHA256_([^@]+)\@}{
    $sha256{$1} // die "no checksum for $1 in $dist\n"
}ge;
die "a placeholder is left in the formula\n" if $formula =~ /\@[A-Z0-9_]+\@/;
print $formula;
