package packages

import (
	"fmt"
	"regexp"
	"strings"
)

var semverPattern = regexp.MustCompile(`^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$`)
var packageNamePattern = regexp.MustCompile(`^[a-z0-9][a-z0-9._-]*$`)

type version struct {
	major, minor, patch string
	pre                 []string
}

func parseVersion(s string) (version, error) {
	m := semverPattern.FindStringSubmatch(s)
	if m == nil {
		return version{}, fmt.Errorf("unsupported Unity package version %q: require exact SemVer", s)
	}
	var v version
	v.major, v.minor, v.patch = m[1], m[2], m[3]
	if m[4] != "" {
		v.pre = strings.Split(m[4], ".")
		for _, id := range v.pre {
			if numeric(id) && len(id) > 1 && id[0] == '0' {
				return v, fmt.Errorf("invalid prerelease identifier in %q", s)
			}
		}
	}
	return v, nil
}
func numeric(s string) bool {
	for _, r := range s {
		if r < '0' || r > '9' {
			return false
		}
	}
	return s != ""
}
func compareVersion(a, b string) int {
	x, _ := parseVersion(a)
	y, _ := parseVersion(b)
	for _, pair := range [][2]string{{x.major, y.major}, {x.minor, y.minor}, {x.patch, y.patch}} {
		if len(pair[0]) < len(pair[1]) || len(pair[0]) == len(pair[1]) && pair[0] < pair[1] {
			return -1
		}
		if len(pair[0]) > len(pair[1]) || len(pair[0]) == len(pair[1]) && pair[0] > pair[1] {
			return 1
		}
	}
	if len(x.pre) == 0 && len(y.pre) > 0 {
		return 1
	}
	if len(y.pre) == 0 && len(x.pre) > 0 {
		return -1
	}
	for i := 0; i < len(x.pre) && i < len(y.pre); i++ {
		a, b := x.pre[i], y.pre[i]
		if a == b {
			continue
		}
		an, bn := numeric(a), numeric(b)
		if an && bn {
			if len(a) < len(b) {
				return -1
			}
			if len(a) > len(b) {
				return 1
			}
		} else if an {
			return -1
		} else if bn {
			return 1
		}
		if a < b {
			return -1
		}
		return 1
	}
	if len(x.pre) < len(y.pre) {
		return -1
	}
	if len(x.pre) > len(y.pre) {
		return 1
	}
	return 0
}
