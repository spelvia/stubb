# Security policy

## Supported versions

Security fixes are released for the latest published version of Stubb.

## Reporting a vulnerability

Please do not open a public issue for security problems. Report them privately
through GitHub instead:
[**Report a vulnerability**](https://github.com/spelvia/stubb/security/advisories/new).

Include what you found, how to reproduce it, and the impact you expect. You will
get an acknowledgement, and we will keep you updated while we work on a fix. Once
a fix is released, we publish a security advisory and credit you unless you
prefer otherwise.

Stubb starts agent CLIs and passes them your application's input, so reports
about agents escaping their configured permissions or sandbox through Stubb, leaked
credentials or environment variables, or untrusted data reaching a process
command line are especially welcome.
