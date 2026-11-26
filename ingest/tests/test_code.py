import textwrap

from accio_ingest.code import MAX_CHARS, parse_code, split_words

JAVA = textwrap.dedent("""\
    package at.example.order;

    import java.util.List;

    /** Where an order came from. */
    public enum OrderSource {
        PARTNER_API("Partner API"),
        WEBSITE("Website");

        private final String label;

        OrderSource(String label) {
            this.label = label;
        }

        public String getLabel() {
            return label;
        }
    }
    """)

SERVICE = textwrap.dedent("""\
    package at.example.order;

    @Service
    @RequiredArgsConstructor
    public class PartnerOrderIntakeService extends BaseService implements OrderIntake {
        private final OrderRepository orders;

        public String getName() {
            return "intake";
        }

        /**
         * Validates and stores an order submitted by a partner.
         */
        @Transactional
        public Order ingest(PartnerOrder order, String partnerId) {
            validate(order);
            Order saved = orders.save(Order.from(order, OrderSource.PARTNER_API));
            events.publish(new OrderCreated(saved.id()));
            return saved;
        }
    }
    """)


def write(root, relative, text):
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    return path


def test_split_words():
    assert split_words("OrderSourceType", "send_sms") == "order source type send sms"
    assert split_words("HTTPClient", "StripePaymentGateway") == "http client stripe payment gateway"


def test_java_enum_outline_lists_constants_and_methods(tmp_path):
    chunks = parse_code(write(tmp_path, "src/main/java/OrderSource.java", JAVA), tmp_path)
    outline = chunks[0]
    assert outline.header.startswith("at.example.order.OrderSource (enum)")
    assert outline.header.endswith("— order source")
    assert 'PARTNER_API("Partner API"), WEBSITE("Website");' in outline.text
    assert "public String getLabel();" in outline.text
    assert outline.text.startswith("package at.example.order;\npublic enum OrderSource {")
    # The getter and the one-line constructor are trivial: no chunk of their own.
    assert len(chunks) == 1
    assert (outline.line, outline.end_line) == (6, 19)


def test_java_method_chunks_are_verbatim_with_javadoc_and_relations(tmp_path):
    path = write(tmp_path, "src/PartnerOrderIntakeService.java", SERVICE)
    chunks = parse_code(path, tmp_path)
    outline, method = chunks[-1], chunks[0]
    assert outline.header.startswith(
        "at.example.order.PartnerOrderIntakeService (class) · @Service @RequiredArgsConstructor "
        "extends BaseService implements OrderIntake"
    )
    assert "private final OrderRepository orders;" in outline.text
    assert method.header.startswith(
        "at.example.order.PartnerOrderIntakeService › public Order ingest(PartnerOrder order, String partnerId)"
    )
    assert "partner order intake service ingest" in method.header
    lines = SERVICE.split("\n")
    assert method.text == "\n".join(lines[method.line - 1:method.end_line])
    assert method.text.startswith("    /**")
    assert method.text.rstrip().endswith("}")
    assert len(chunks) == 2  # getName() is trivial


def test_long_java_method_splits_at_lines(tmp_path):
    body = "\n".join(f"        call{index}(argument{index});" for index in range(200))
    text = f"package p;\nclass Big {{\n    void run() {{\n{body}\n    }}\n}}\n"
    chunks = parse_code(write(tmp_path, "Big.java", text), tmp_path)
    parts = [chunk for chunk in chunks if "run()" in chunk.header]
    assert len(parts) > 1
    assert all(len(part.text) <= MAX_CHARS for part in parts)
    assert parts[1].line == parts[0].end_line + 1


def test_vue_script_and_template_keep_file_lines(tmp_path):
    script = "\n".join(f"function helper{index}() {{\n  return {index} * 2\n}}" for index in range(80))
    text = (
        "<template>\n  <LeadForm :source=\"source\" />\n</template>\n\n"
        f"<script setup>\nimport LeadForm from './LeadForm.vue'\n{script}\n</script>\n"
    )
    path = write(tmp_path, "src/views/LeadView.vue", text)
    chunks = parse_code(path, tmp_path)
    lines = text.split("\n")
    template = next(chunk for chunk in chunks if "template" in chunk.header)
    assert template.line == 2 and "LeadForm" in template.text
    outline = next(chunk for chunk in chunks if "module outline" in chunk.header)
    assert "import LeadForm from './LeadForm.vue'" in outline.text
    code = [chunk for chunk in chunks if chunk not in (template, outline)]
    assert len(code) > 1
    for chunk in code:
        assert chunk.header.startswith("src/views/LeadView.vue (setup)")
        assert chunk.text == "\n".join(lines[chunk.line - 1:chunk.end_line])
    assert any("helper79" in chunk.text for chunk in code)


def test_options_api_object_splits_by_property(tmp_path):
    methods = ",\n".join(
        f"    method{index}() {{\n      return this.items.filter(item => item.id === {index})\n    }}"
        for index in range(40)
    )
    text = f"export default {{\n  name: 'LeadList',\n  methods: {{\n{methods}\n  }}\n}}\n"
    chunks = parse_code(write(tmp_path, "list.js", text), tmp_path)
    assert any("default export" in chunk.header for chunk in chunks)
    assert "name: 'LeadList'" in chunks[0].text or "name: 'LeadList'" in chunks[1].text
    joined = "\n".join(chunk.text for chunk in chunks)
    assert "method0()" in joined and "method39()" in joined


def test_sql_names_tables_and_config_names_keys(tmp_path):
    sql = "CREATE TABLE order_source (id bigint primary key);\nALTER TABLE order ADD COLUMN source_id bigint;\n"
    chunk = parse_code(write(tmp_path, "db/V1__order_source.sql", sql), tmp_path)[0]
    assert chunk.header == "db/V1__order_source.sql › order_source, order"
    assert (chunk.line, chunk.end_line) == (1, 2)
    yaml = "spring:\n  application:\n    name: shop\nstripe:\n  enabled: true\n"
    config = parse_code(write(tmp_path, "src/main/resources/application.yml", yaml), tmp_path)[0]
    assert config.header == "src/main/resources/application.yml › spring, stripe"


def test_python_and_markdown(tmp_path):
    python = "import os\n\n\nclass Mailer:\n    def send(self, to):\n        return os.environ['SMTP']\n"
    chunk = parse_code(write(tmp_path, "tools/mail.py", python), tmp_path)[0]
    assert chunk.header.startswith("tools/mail.py")
    assert "class Mailer" in chunk.text
    readme = parse_code(write(tmp_path, "README.md", "# Shop\n\nOnline store."), tmp_path)
    assert readme[0].header == "README.md › Shop"
    assert parse_code(write(tmp_path, "empty.js", "\n"), tmp_path) == []
