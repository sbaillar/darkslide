# Sourced by the scripts (bash). The environment variables are named LT_* since the
# rename; the DS_* names they had before are still accepted for one release, so a CI
# configuration or a habit doesn't break silently. LT_* wins when both are set.
# Migration source: the DS_ prefix.
for _lt_v in $(compgen -e); do
  if [[ "$_lt_v" == DS_* ]]; then
    _lt_n="LT_${_lt_v#DS_}"
    [[ -n "${!_lt_n+x}" ]] || export "$_lt_n=${!_lt_v}"
  fi
done
unset _lt_v _lt_n
