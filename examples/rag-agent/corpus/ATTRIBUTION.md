# Where this corpus comes from

Both examples in `examples/rag-agent/` search the Wikipedia articles below, taken from the
20251222 CirrusSearch dumps of the English and Simple English Wikipedias. The text is
unchanged. The command line example splits it into passages of about 1,100 characters with the
article title in front, and the Rust example into overlapping chunks of about 1,000 characters.

**Licence: [CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/).** The text is used and
redistributed here under that licence, and anything derived from it carries the same one. Each
article's authors are named in its page history, which the link beside it reaches.

80 articles, 2,374,066 characters. `../cli-example/scripts/select-corpus.py` holds the list of titles and
the reason it is a list rather than a rule.

| article | wiki | characters |
|---|---|---:|
| [Alcidamas](https://en.wikipedia.org/wiki/Alcidamas) | English | 4,025 |
| [Alexander of Aphrodisias](https://en.wikipedia.org/wiki/Alexander_of_Aphrodisias) | English | 17,305 |
| [Allegory of the cave](https://en.wikipedia.org/wiki/Allegory_of_the_cave) | English | 18,191 |
| [Ammonius Hermiae](https://en.wikipedia.org/wiki/Ammonius_Hermiae) | English | 7,743 |
| [Ammonius Saccas](https://en.wikipedia.org/wiki/Ammonius_Saccas) | English | 13,535 |
| [Anacharsis](https://en.wikipedia.org/wiki/Anacharsis) | English | 5,374 |
| [Anaxagoras](https://en.wikipedia.org/wiki/Anaxagoras) | English | 18,195 |
| [Anaxarchus](https://en.wikipedia.org/wiki/Anaxarchus) | English | 4,276 |
| [Anaximander](https://en.wikipedia.org/wiki/Anaximander) | English | 44,627 |
| [Anaximenes of Lampsacus](https://en.wikipedia.org/wiki/Anaximenes_of_Lampsacus) | English | 6,235 |
| [Anaximenes of Miletus](https://en.wikipedia.org/wiki/Anaximenes_of_Miletus) | English | 20,153 |
| [Ancient Greek philosophy](https://simple.wikipedia.org/wiki/Ancient_Greek_philosophy) | Simple English | 8,026 |
| [Antisthenes](https://en.wikipedia.org/wiki/Antisthenes) | English | 13,473 |
| [Aristotle](https://en.wikipedia.org/wiki/Aristotle) | English | 103,802 |
| [Aristoxenus](https://en.wikipedia.org/wiki/Aristoxenus) | English | 15,472 |
| [Atomism](https://simple.wikipedia.org/wiki/Atomism) | Simple English | 1,778 |
| [Chrysippus](https://en.wikipedia.org/wiki/Chrysippus) | English | 35,417 |
| [Cicero](https://en.wikipedia.org/wiki/Cicero) | English | 73,856 |
| [Classical Athens](https://simple.wikipedia.org/wiki/Classical_Athens) | Simple English | 7,656 |
| [Classical element](https://en.wikipedia.org/wiki/Classical_element) | English | 29,785 |
| [Claudius Aelianus](https://en.wikipedia.org/wiki/Claudius_Aelianus) | English | 12,311 |
| [Crates of Thebes](https://en.wikipedia.org/wiki/Crates_of_Thebes) | English | 11,152 |
| [Demiurge](https://en.wikipedia.org/wiki/Demiurge) | English | 35,430 |
| [Democritus](https://en.wikipedia.org/wiki/Democritus) | English | 35,206 |
| [Dialectic](https://en.wikipedia.org/wiki/Dialectic) | English | 31,937 |
| [Dicaearchus](https://en.wikipedia.org/wiki/Dicaearchus) | English | 16,680 |
| [Diogenes Laertius](https://en.wikipedia.org/wiki/Diogenes_Laertius) | English | 23,332 |
| [Diogenes of Sinope](https://simple.wikipedia.org/wiki/Diogenes_of_Sinope) | Simple English | 2,324 |
| [Eleaticism](https://simple.wikipedia.org/wiki/Eleaticism) | Simple English | 1,894 |
| [Empedocles](https://en.wikipedia.org/wiki/Empedocles) | English | 20,182 |
| [Epictetus](https://en.wikipedia.org/wiki/Epictetus) | English | 14,525 |
| [Epicureanism](https://en.wikipedia.org/wiki/Epicureanism) | English | 44,491 |
| [Epicurus](https://en.wikipedia.org/wiki/Epicurus) | English | 62,013 |
| [Epimenides](https://en.wikipedia.org/wiki/Epimenides) | English | 5,549 |
| [Epistemology](https://en.wikipedia.org/wiki/Epistemology) | English | 131,784 |
| [Ethics](https://en.wikipedia.org/wiki/Ethics) | English | 128,116 |
| [Eubulides](https://en.wikipedia.org/wiki/Eubulides) | English | 4,971 |
| [Eudoxus of Cnidus](https://en.wikipedia.org/wiki/Eudoxus_of_Cnidus) | English | 17,742 |
| [Favorinus](https://en.wikipedia.org/wiki/Favorinus) | English | 9,108 |
| [Genus–differentia definition](https://en.wikipedia.org/wiki/Genus–differentia_definition) | English | 7,638 |
| [Gorgias](https://en.wikipedia.org/wiki/Gorgias) | English | 32,015 |
| [Hellenistic philosophy](https://simple.wikipedia.org/wiki/Hellenistic_philosophy) | Simple English | 2,580 |
| [Heraclitus](https://en.wikipedia.org/wiki/Heraclitus) | English | 86,681 |
| [Hierocles of Alexandria](https://en.wikipedia.org/wiki/Hierocles_of_Alexandria) | English | 3,493 |
| [Hippias](https://en.wikipedia.org/wiki/Hippias) | English | 6,411 |
| [Hypatia](https://en.wikipedia.org/wiki/Hypatia) | English | 65,312 |
| [Leucippus](https://en.wikipedia.org/wiki/Leucippus) | English | 24,192 |
| [Logic](https://simple.wikipedia.org/wiki/Logic) | Simple English | 5,123 |
| [Lucretius](https://en.wikipedia.org/wiki/Lucretius) | English | 13,819 |
| [Marcus Aurelius](https://en.wikipedia.org/wiki/Marcus_Aurelius) | English | 104,607 |
| [Metaphysics](https://en.wikipedia.org/wiki/Metaphysics) | English | 106,110 |
| [Metempsychosis](https://en.wikipedia.org/wiki/Metempsychosis) | English | 7,486 |
| [Nicomachean Ethics](https://simple.wikipedia.org/wiki/Nicomachean_Ethics) | Simple English | 2,539 |
| [Parmenides](https://en.wikipedia.org/wiki/Parmenides) | English | 97,663 |
| [Plato](https://en.wikipedia.org/wiki/Plato) | English | 37,983 |
| [Plotinus](https://en.wikipedia.org/wiki/Plotinus) | English | 55,437 |
| [Plutarch](https://en.wikipedia.org/wiki/Plutarch) | English | 32,412 |
| [Posidonius](https://en.wikipedia.org/wiki/Posidonius) | English | 27,829 |
| [Pre-Socratic philosophy](https://en.wikipedia.org/wiki/Pre-Socratic_philosophy) | English | 69,023 |
| [Proclus](https://en.wikipedia.org/wiki/Proclus) | English | 21,659 |
| [Prodicus](https://en.wikipedia.org/wiki/Prodicus) | English | 9,808 |
| [Protagoras](https://en.wikipedia.org/wiki/Protagoras) | English | 15,330 |
| [Pythagoras](https://en.wikipedia.org/wiki/Pythagoras) | English | 64,987 |
| [Seneca the Younger](https://en.wikipedia.org/wiki/Seneca_the_Younger) | English | 42,486 |
| [Simplicius of Cilicia](https://en.wikipedia.org/wiki/Simplicius_of_Cilicia) | English | 45,740 |
| [Skepticism](https://en.wikipedia.org/wiki/Skepticism) | English | 26,671 |
| [Socrates](https://simple.wikipedia.org/wiki/Socrates) | Simple English | 9,308 |
| [Socratic method](https://en.wikipedia.org/wiki/Socratic_method) | English | 24,711 |
| [Sophist](https://en.wikipedia.org/wiki/Sophist) | English | 31,717 |
| [Stoicism](https://simple.wikipedia.org/wiki/Stoicism) | Simple English | 1,446 |
| [Substance theory](https://en.wikipedia.org/wiki/Substance_theory) | English | 33,763 |
| [Thales of Miletus](https://en.wikipedia.org/wiki/Thales_of_Miletus) | English | 44,594 |
| [Theophrastus](https://en.wikipedia.org/wiki/Theophrastus) | English | 43,833 |
| [Theory of forms](https://simple.wikipedia.org/wiki/Theory_of_forms) | Simple English | 1,333 |
| [Theurgy](https://en.wikipedia.org/wiki/Theurgy) | English | 8,905 |
| [Virtue](https://en.wikipedia.org/wiki/Virtue) | English | 35,556 |
| [Xenophanes](https://simple.wikipedia.org/wiki/Xenophanes) | Simple English | 2,255 |
| [Xenophon](https://en.wikipedia.org/wiki/Xenophon) | English | 36,704 |
| [Zeno of Citium](https://en.wikipedia.org/wiki/Zeno_of_Citium) | English | 25,007 |
| [Zeno of Elea](https://simple.wikipedia.org/wiki/Zeno_of_Elea) | Simple English | 2,229 |
