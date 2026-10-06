module unitymicroeditor/worker

go 1.24

require unitymicroeditor/project v0.0.0

replace unitymicroeditor/project => ../GoProject

require github.com/zeebo/xxh3 v1.0.2

require (
	github.com/klauspost/cpuid/v2 v2.0.9 // indirect
	gopkg.in/yaml.v3 v3.0.1 // indirect
)
