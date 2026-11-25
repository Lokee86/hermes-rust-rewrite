#!/bin/sh
# Fail if a commit in <base>..<head> was made under an unset identity, which git fills in with the
# computer's account name and host name.
set -eu
[ "$#" -eq 2 ] || { echo "usage: $0 <base> <head>" >&2; exit 2; }
commits=$(git rev-list "$1..$2")
bad=
for commit in $commits; do
    for email in $(git show -s --format='%ae %ce' "$commit"); do
        case "$email" in
            *@*.local | *@*.lan | *@*.internal | *@*.home | *@localhost | *@*.localdomain | *"(none)"*) bad="$bad $commit"; break ;;
            *@*.*) ;;
            *) bad="$bad $commit"; break ;;
        esac
    done
done
[ -z "$bad" ] && exit 0
# The commit only: the log must not repeat the name.
for commit in $bad; do
    echo "$commit was made under a computer's own identity. Set user.name and user.email, then rewrite it." >&2
done
exit 1
