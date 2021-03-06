.PHONY: all clean
all:
	$(MAKE) sources
	$(MAKE) thesis.pdf

clean:
	rm -f graphs/*.pdf
	rm -f benchmarks/results/*/parsed.pickle
	rm -f thesis.pdf proposal.pdf
	rm -f thesis.gray.pdf

abstract.pdf: abstract.tex

thesis.pdf: titlepage.pdf abstract.pdf \
            thesis.tex bibliography.bib \
	    00-acknowledgements.tex \
	    01-introduction.tex \
	    02-noria.tex \
	    03-partial.tex \
	    04-correctness.tex \
	    05-implementation.tex \
	    06-evaluation.tex \
	    07-related-work.tex \
	    08-discussion.tex \
	    09-future-work.tex \
	    A1-simpler-terms.tex \
	    graphs/lobsters-throughput.pdf \
	    graphs/lobsters-memory.pdf \
	    graphs/lobsters-opmem.pdf \
	    graphs/lobsters-memlimit-cdf.pdf \
	    graphs/lobsters-durability-cdf.pdf \
	    graphs/lobsters-timeline.pdf \
	    graphs/vote-formula.pdf \
	    graphs/vote-migration.pdf \
	    graphs/vote-throughput-memlimit.pdf \
	    graphs/vote-redis.pdf \
	    diagrams/Motivation\ Classic\ DB.pdf \
	    diagrams/Motivation\ Ad\ Hoc\ Cache.pdf \
	    diagrams/Motivation\ Materialized\ Views.pdf \
	    diagrams/Example\ Execution.pdf \
	    diagrams/Key\ Provenance.pdf \
	    diagrams/Chained\ Unions.pdf \
	    diagrams/Indexing.pdf
	latexmk -shell-escape -pdf thesis.tex

thesis.gray.pdf: thesis.pdf
	gs \
	    -sOutputFile=$@ \
	    -sDEVICE=pdfwrite \
	    -sColorConversionStrategy=Gray \
	    -dProcessColorModel=/DeviceGray \
	    -dCompatibilityLevel=1.4 \
	    -dNOPAUSE \
	    -dBATCH \
	    thesis.pdf

%.pdf: %.tex
	latexmk -pdf $<

RESULTS = lobsters lobsters-mysql vote vote-migration vote-redis vote-formula vote-nojoin

.PHONY: sources $(RESULTS)

sources: $(RESULTS)

$(RESULTS):
	$(MAKE) -C $(addprefix benchmarks/results/, $@)

benchmarks/results/%/parsed.pickle: sources

graphs/lobsters-%.pdf: graphs/common.py benchmarks/results/lobsters/parsed.pickle graphs/lobsters-%.py
	python3 graphs/lobsters-$*.py graphs/lobsters-$*

graphs/lobsters-throughput.pdf: graphs/common.py graphs/lobsters-throughput.py \
	benchmarks/results/lobsters/parsed.pickle \
	benchmarks/results/lobsters-mysql/parsed.pickle
	python3 graphs/lobsters-throughput.py graphs/lobsters-throughput

graphs/vote-migration.pdf: graphs/common.py benchmarks/results/vote-migration/parsed.pickle graphs/vote-migration.py
	python3 graphs/vote-migration.py graphs/vote-migration

graphs/vote-redis.pdf: graphs/common.py graphs/vote-redis.py \
	benchmarks/results/vote-nojoin/parsed.pickle \
	benchmarks/results/vote-redis/parsed.pickle
	python3 graphs/vote-redis.py graphs/vote-redis

graphs/vote-formula.pdf: graphs/common.py graphs/vote-formula.py \
	benchmarks/results/vote-formula/results.log \
	benchmarks/results/vote/parsed.pickle
	python3 graphs/vote-formula.py graphs/vote-formula

graphs/vote-%.pdf: graphs/common.py benchmarks/results/vote/parsed.pickle graphs/vote-%.py
	python3 graphs/vote-$*.py graphs/vote-$*
