import io
import json
import unittest
from analyze import analyze_csv


def csv_rows(records):
    head = 'size,range_len,iterations,round,mode,elapsed_ns,checksum\n'
    return io.StringIO(head + '\n'.join(records) + '\n')


class AnalyzeTests(unittest.TestCase):
    def test_paired_ratios_and_json_structure(self):
        rows = csv_rows([
            '128,64,100,0,copy,200,300',
            '128,64,100,0,view,100,300',
            '128,64,100,1,view,80,300',
            '128,64,100,1,copy,200,300',
            '128,64,100,2,copy,200,300',
            '128,64,100,2,view,120,300',
        ])
        result = analyze_csv(rows)
        self.assertEqual(len(result['workloads']), 1)
        one = result['workloads'][0]
        self.assertEqual(one['rounds'], 3)
        self.assertEqual(one['median_view_ns'], 100)
        self.assertEqual(one['median_copy_ns'], 200)
        self.assertAlmostEqual(one['paired_median_ratio'], .5)
        lo, hi = one['bootstrap_95_ratio']
        self.assertTrue(.4 <= lo <= hi <= .6)
        self.assertIn('diagnostic', result['interpretation'].lower())
        self.assertEqual(json.loads(json.dumps(result))['workloads'][0]['size'], 128)

    def test_reject_checksum_mismatch(self):
        with self.assertRaisesRegex(ValueError, 'checksum'):
            analyze_csv(csv_rows([
                '8,4,1,0,view,10,2',
                '8,4,1,0,copy,10,3',
            ]))

    def test_reject_missing_or_duplicate_pairs(self):
        with self.assertRaisesRegex(ValueError, 'incomplete'):
            analyze_csv(csv_rows(['8,4,1,0,view,10,2']))
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            analyze_csv(csv_rows([
                '8,4,1,0,view,10,2',
                '8,4,1,0,view,12,2',
                '8,4,1,0,copy,11,2',
            ]))

    def test_reject_nonpositive_duration_and_invalid_mode(self):
        with self.assertRaisesRegex(ValueError, 'elapsed_ns'):
            analyze_csv(csv_rows([
                '8,4,1,0,view,0,2',
                '8,4,1,0,copy,9,2',
            ]))
        with self.assertRaisesRegex(ValueError, 'mode'):
            analyze_csv(csv_rows(['8,4,1,0,compile,4,2']))

    def test_reject_insufficient_rounds_and_missing_fields(self):
        with self.assertRaisesRegex(ValueError, 'at least two'):
            analyze_csv(csv_rows([
                '8,4,1,0,view,1,2',
                '8,4,1,0,copy,2,2',
            ]))
        with self.assertRaisesRegex(ValueError, 'columns'):
            analyze_csv(io.StringIO('size,mode\n128,copy\n'))

    def test_multiple_workload_sizes_are_independent(self):
        rows = csv_rows([
            '8,4,1,0,view,100,1',
            '8,4,1,0,copy,200,1',
            '8,4,1,1,view,80,1',
            '8,4,1,1,copy,200,1',
            '128,64,1,0,view,25,2',
            '128,64,1,0,copy,50,2',
            '128,64,1,1,view,20,2',
            '128,64,1,1,copy,50,2',
        ])
        self.assertEqual([w['size'] for w in analyze_csv(rows)['workloads']], [8, 128])


if __name__ == '__main__':
    unittest.main()
