"""Tests for the repository's fail-closed GitHub issue-form validator."""
import importlib.util
import pathlib
import tempfile
import unittest

MODULE = pathlib.Path(__file__).resolve().parents[1] / 'validate_issue_forms.py'
spec = importlib.util.spec_from_file_location('validate_issue_forms', MODULE)
validate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(validate)


class IssueFormValidationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name)
        self.directory = self.root / '.github' / 'ISSUE_TEMPLATE'
        self.directory.mkdir(parents=True)

    def write(self, filename, text):
        path = self.directory / filename
        path.write_text(text, encoding='utf-8')
        return path

    def test_valid_issue_form_and_config(self):
        self.write('bug_report.yml', '''name: Bug Report
description: Report a bug
body:
  - type: markdown
    attributes:
      value: Before reporting, check duplicates.
  - type: textarea
    id: repro
    attributes:
      label: Steps to reproduce
    validations:
      required: true
''')
        self.write('config.yml', '''blank_issues_enabled: false
contact_links:
  - name: Help
    url: https://example.org/help
    about: Contact support
''')
        self.assertEqual([], validate.validate_templates(self.directory))

    def test_invalid_yaml_is_red_with_location(self):
        self.write('invalid.yml', 'name: Bad\ndescription: Broken\nbody: [oops\n')
        result = validate.validate_templates(self.directory)
        self.assertTrue(result)
        self.assertIn('invalid.yml', result[0])
        self.assertIn('YAML', result[0])

    def test_missing_required_form_metadata_fails(self):
        self.write('bad.yml', 'name: Bug\nbody: []\n')
        errors = '\n'.join(validate.validate_templates(self.directory))
        self.assertIn('description', errors)
        self.assertIn('body', errors)

    def test_form_control_requires_valid_type_and_attributes(self):
        self.write('bad.yaml', '''name: Bug
description: Broken
body:
  - type: command
    id: command
    attributes:
      label: Execute
  - type: input
    id: detail
    attributes: {}
''')
        errors = '\n'.join(validate.validate_templates(self.directory))
        self.assertIn('type', errors)
        self.assertIn('label', errors)

    def test_duplicate_control_ids_are_rejected(self):
        self.write('bad.yml', '''name: Bug
description: Broken
body:
  - type: input
    id: version
    attributes: {label: Version}
  - type: textarea
    id: version
    attributes: {label: Reproduction}
''')
        self.assertIn('duplicate', '\n'.join(validate.validate_templates(self.directory)))

    def test_duplicate_yaml_key_is_rejected(self):
        self.write('bad.yml', "name: First\nname: Second\ndescription: Bug\nbody: []\n")
        self.assertIn('duplicate YAML mapping key', '\n'.join(validate.validate_templates(self.directory)))

    def test_issue_template_symlink_is_rejected(self):
        target = self.write('real.yml', "name: First\ndescription: Bug\nbody: [{type: markdown, attributes: {value: Hi}}]\n")
        (self.directory / 'linked.yml').symlink_to(target)
        self.assertIn('symlink', '\n'.join(validate.validate_templates(self.directory)))

    def test_required_validation_must_be_boolean(self):
        self.write('bad.yml', '''name: Bug
description: Broken
body:
  - type: input
    id: version
    attributes: {label: Version}
    validations: {required: 'true'}
''')
        self.assertIn('boolean', '\n'.join(validate.validate_templates(self.directory)))

    def test_unknown_yaml_tags_fail_closed(self):
        self.write('bad.yml', 'name: !!python/object/apply:os.system [whoami]\n')
        self.assertIn('YAML', '\n'.join(validate.validate_templates(self.directory)))

    def test_config_schema_is_checked_separately(self):
        self.write('config.yaml', '''blank_issues_enabled: 'yes'
contact_links:
  - url: not-a-url
    name: Discussion
''')
        errors = '\n'.join(validate.validate_templates(self.directory))
        self.assertIn('boolean', errors)
        self.assertIn('about', errors)
        self.assertIn('https://', errors)

    def test_main_fails_when_invalid_and_succeeds_when_valid(self):
        p = self.write('bug_report.yml', 'name: Bug\n')
        self.assertEqual(1, validate.main(['--root', str(self.root)]))
        p.write_text('''name: Bug
description: Broken
body:
  - type: markdown
    attributes: {value: Some text}
''', encoding='utf-8')
        self.assertEqual(0, validate.main(['--root', str(self.root)]))

    def test_no_template_files_fails_closed(self):
        self.assertTrue(validate.validate_templates(self.directory))


if __name__ == '__main__':
    unittest.main()
