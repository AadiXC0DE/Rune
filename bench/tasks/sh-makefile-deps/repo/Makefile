OUTS = out/a.out out/b.out out/c.out

all: $(OUTS) out/index.txt

out:
	mkdir -p out

out/a.out: src/a.txt | out
	cat include/common.txt src/a.txt > $@

out/b.out: src/b.txt | out
	cat include/common.txt src/b.txt > $@

out/c.out: src/c.txt | out
	cat src/c.txt > $@

.PHONY: out/index.txt
out/index.txt: $(OUTS)
	printf '%s\n' $(notdir $(OUTS)) > $@

clean:
	rm -rf out

.PHONY: all clean
