class Main {
    static long add(long x, long y) {
        return x + y;
    }

    public static void main(String[] args) {
        long sum = 0;
        long i = 0;
        while (i < 9_000_000L) {
            sum = add(sum, i * 3L - i / 7L);
            if (sum > 1_000_000_000L) {
                sum -= 1_000_000_000L;
            }
            i++;
        }
        System.out.println(sum);
    }
}
